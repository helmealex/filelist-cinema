const $ = (sel) => document.querySelector(sel);

const state = {
  config: null,
  build: null,
  movies: new Map(), // id -> movie (from rows and search)
  groups: new Map(), // group key (IMDb id or title|year) -> Set of release ids of the same movie
  downloads: new Map(), // id -> download status
  progress: {}, // id -> { t, duration, index, updatedAt }
  detailsId: null,
  player: null, // { id, index, quality, compat, offset, duration, subKey, subDelay, tracks, pendingSeek }
  search: { page: 0, query: '', cats: '', loading: false, hasMore: true, cards: new Map() },
};

// ---------- helpers ----------
const esc = (s) =>
  String(s ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]);

const store = {
  get(k) {
    try {
      return localStorage.getItem(k);
    } catch {
      return null;
    }
  },
  set(k, v) {
    try {
      localStorage.setItem(k, v);
    } catch {
      /* storage unavailable */
    }
  },
};

function parseReleaseName(name) {
  const clean = String(name).replace(/[._]/g, ' ').replace(/\s+/g, ' ').trim();
  // TV releases: "Show S02E05 …", "Show 2024 S01 …", "Show Season 1 …"
  const ep = clean.match(/^(.+?)\s(?:S\d{1,2}(?:\s?E\d{1,3})?|Season\s\d+|Complete\sSeries)\b/i);
  if (ep) {
    const y = ep[1].match(/^(.+?)\s+[([]?((?:19|20)\d{2})[)\]]?$/);
    return y ? { title: y[1].trim(), year: y[2] } : { title: ep[1].trim(), year: null };
  }
  const m = clean.match(/^(.+?)\s+[([]?((?:19|20)\d{2})[)\]]?(?:\s|$)/);
  if (m) return { title: m[1].trim(), year: m[2] };
  return { title: clean.split(/\s(?:\d{3,4}p|BluRay|WEB|HDTV|DVDRip|x26[45]|HEVC)\b/i)[0], year: null };
}

/** Badges derived from the release name, e.g. 4K · HDR · Remux · DTS-HD MA · RO subs. */
function releaseTags(name = '', category = '') {
  const n = String(name).replace(/[._]/g, ' ');
  const tags = [];
  if (/\b(2160p|4K|UHD)\b/i.test(n)) tags.push('4K');
  else if (/\b1080p\b/i.test(n)) tags.push('1080p');
  else if (/\b720p\b/i.test(n)) tags.push('720p');
  if (/\bHDR10\+/i.test(n)) tags.push('HDR10+');
  else if (/\bHDR(10)?\b/i.test(n)) tags.push('HDR');
  if (/\b(DV|DoVi|Dolby Vision)\b/i.test(n)) tags.push('Dolby Vision');
  if (/\bRemux\b/i.test(n)) tags.push('Remux');
  else if (/\bBlu-?Ray\b/i.test(n)) tags.push('Blu-ray');
  else if (/\bWEB(-?DL|-?Rip)?\b/i.test(n)) tags.push('WEB');
  if (/\bAtmos\b/i.test(n)) tags.push('Atmos');
  const audio = String(name).match(/\b(DTS-HD[ .]MA|DTS-X|DTS|TrueHD|DDP|DD\+|DD|EAC3|AC3|AAC)[ .]?(\d[ .]\d)?/i);
  if (audio) tags.push(`${audio[1].replace('.', ' ')}${audio[2] ? ` ${audio[2].replace(' ', '.')}` : ''}`);
  if (/x265|HEVC|H[ .]?265/i.test(String(name))) tags.push('HEVC');
  if (/-RO\b/i.test(category) || /\bRO(SUB)?\b/.test(n)) tags.push('RO subs');
  return tags;
}
const qualityBadge = (name) => releaseTags(name).find((t) => ['4K', '1080p', '720p'].includes(t)) || null;
const QUALITY_RANK = { '4K': 3, '1080p': 2, '720p': 1 };
const qualityRank = (m) => QUALITY_RANK[qualityBadge(m.name)] || 0;

// FileList lists every release (1080p, 720p, 4K, …) separately; group them so each movie shows once.
function groupKey(m) {
  if (m.imdb) return m.imdb;
  const { title, year } = parseReleaseName(m.name);
  return `${title.toLowerCase().replace(/[^a-z0-9]+/g, ' ').trim()}|${year || ''}`;
}

function rememberMovie(m) {
  state.movies.set(m.id, { ...state.movies.get(m.id), ...m });
  const key = groupKey(m);
  if (!state.groups.has(key)) state.groups.set(key, new Set());
  state.groups.get(key).add(m.id);
  return key;
}

/** All known releases of the same movie, best first (downloaded, then quality, then seeders). */
function releasesOf(id) {
  const m = state.movies.get(id);
  if (!m) return [];
  const ids = state.groups.get(groupKey(m)) || new Set([id]);
  return [...ids]
    .map((i) => state.movies.get(i))
    .filter(Boolean)
    .sort(
      (a, b) =>
        Number(state.downloads.has(b.id)) - Number(state.downloads.has(a.id)) ||
        qualityRank(b) - qualityRank(a) ||
        (b.seeders || 0) - (a.seeders || 0),
    );
}

/** Add movies to a container of cards, merging releases of a movie into a single card. */
function appendGrouped(container, cardsByKey, items) {
  for (const m of items) {
    const key = rememberMovie(m);
    const existing = cardsByKey.get(key);
    const best = releasesOf(m.id)[0];
    if (!existing) {
      const el = card(best, { grouped: true });
      cardsByKey.set(key, el);
      container.append(el);
    } else {
      const refreshed = card(best, { grouped: true });
      const hadFocus = existing === document.activeElement;
      existing.replaceWith(refreshed);
      cardsByKey.set(key, refreshed);
      if (hadFocus) refreshed.focus({ preventScroll: true });
    }
  }
}

const posterUrl = (m) => m.poster || fallbackPoster(m);
const fallbackPoster = (m) => `/api/poster?${new URLSearchParams({ imdb: m.imdb || '', name: m.name })}`;

function bytes(n) {
  if (!n) return '0 B';
  const u = ['B', 'KB', 'MB', 'GB', 'TB'];
  const i = Math.min(u.length - 1, Math.floor(Math.log(n) / Math.log(1024)));
  return `${(n / 1024 ** i).toFixed(i >= 2 ? 1 : 0)} ${u[i]}`;
}
const pct = (p) => `${Math.floor((p || 0) * 1000) / 10}%`;
function clock(sec) {
  sec = Math.max(0, Math.floor(sec || 0));
  const h = Math.floor(sec / 3600);
  const m = Math.floor((sec % 3600) / 60);
  const s = String(sec % 60).padStart(2, '0');
  return h ? `${h}:${String(m).padStart(2, '0')}:${s}` : `${m}:${s}`;
}
function eta(ms) {
  if (!ms) return '';
  const s = ms / 1000;
  return s > 3600 ? `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m` : `${Math.ceil(s / 60)}m`;
}

async function api(url, opts) {
  const res = await fetch(url, opts);
  const data = await res.json().catch(() => ({}));
  if (!res.ok) throw new Error(data.error || `HTTP ${res.status}`);
  return data;
}

let toastTimer;
function toast(text, isError = false) {
  const el = $('#toast');
  el.textContent = text;
  el.classList.toggle('error', isError);
  el.classList.add('show');
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => el.classList.remove('show'), isError ? 6000 : 3500);
}

const ICON_CHECK = '<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M5 12.5l4.5 4.5L19 7.5"/></svg>';
const ICON_DOWN = '<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M12 4v12M6 11l6 6 6-6M5 20h14"/></svg>';
const ICON_PLAY = '<span class="play-icon" aria-hidden="true"></span>';

function libraryToMovie(d) {
  return { id: d.id, name: d.name, imdb: d.imdb, poster: d.poster, size: d.length ? bytes(d.length) : '', category: '' };
}

/** A movie can be resumed when it has been watched for a while but not finished. */
function resumePoint(id) {
  const pr = state.progress[id];
  if (!pr || pr.t < 30) return null;
  if (pr.duration && pr.t > pr.duration * 0.95) return null;
  return pr;
}

// ---------- cards ----------
function card(m, { watched, grouped } = {}) {
  const { title, year } = parseReleaseName(m.name);
  const releases = grouped ? releasesOf(m.id) : [m];
  const q = qualityBadge(releases.reduce((best, r) => (qualityRank(r) > qualityRank(best) ? r : best), m).name);
  const freeleech = releases.some((r) => r.freeleech);
  const seeders = releases.some((r) => r.seeders != null) ? releases.reduce((sum, r) => sum + (r.seeders || 0), 0) : null;
  const el = document.createElement('button');
  el.type = 'button';
  el.className = 'card';
  el.dataset.id = m.id;
  el.setAttribute('aria-label', `${title}${year ? ` (${year})` : ''}`);
  el.innerHTML = `
    <div class="poster">
      <img class="loading" loading="lazy" alt="" src="${esc(posterUrl(m))}" />
      <div class="badges">
        ${q ? `<span class="badge ${q === '4K' ? 'k4' : ''}">${q}</span>` : ''}
        ${freeleech ? '<span class="badge free">FREE</span>' : ''}
        <span class="owned hidden" title="Downloaded">${ICON_CHECK}</span>
      </div>
      <div class="info">
        <div class="title">${esc(title)}</div>
        <div class="sub">
          ${year ? `<span>${year}</span>` : ''}
          ${releases.length > 1 ? `<span class="versions-count">${releases.length} versions</span>` : m.size ? `<span>${esc(m.size)}</span>` : ''}
          ${seeders != null ? `<span class="seed">▲ ${seeders}</span>` : ''}
        </div>
      </div>
      <div class="bar hidden"><div></div></div>
    </div>`;
  if (watched != null) el.dataset.watched = String(watched);
  if (grouped) el.dataset.grouped = '1';
  const img = el.querySelector('img');
  img.onload = () => img.classList.remove('loading');
  img.onerror = () => {
    if (!img.dataset.fallback) {
      img.dataset.fallback = '1';
      img.src = fallbackPoster(m);
    }
  };
  el.addEventListener('click', () => openDetails(m.id));
  updateCard(el);
  return el;
}

/** Downloaded tick + progress bar (watch progress in "Continue watching", else download progress). */
function updateCard(el) {
  const owned = el.dataset.grouped ? releasesOf(el.dataset.id).find((r) => state.downloads.has(r.id)) : null;
  const dl = state.downloads.get(owned?.id ?? el.dataset.id);
  el.querySelector('.owned').classList.toggle('hidden', !dl);
  const bar = el.querySelector('.bar');
  const fraction = el.dataset.watched != null ? Number(el.dataset.watched) : dl && !dl.done ? dl.progress : null;
  bar.classList.toggle('hidden', fraction == null);
  if (fraction != null) bar.firstElementChild.style.width = pct(fraction);
}
const updateAllCards = () => document.querySelectorAll('.card[data-id]').forEach(updateCard);

function skeletons(n) {
  return Array.from({ length: n }, () => {
    const s = document.createElement('div');
    s.className = 'card skeleton';
    return s;
  });
}

// ---------- home rows ----------
const ROWS = [
  { key: 'continue', title: 'Continue watching', kind: 'continue' },
  { key: 'library', title: 'My downloads', kind: 'library' },
  { key: 'hd', title: 'New in HD', cats: '4' },
  { key: 'ro', title: 'HD with Romanian subtitles', cats: '19' },
  { key: 'uhd', title: '4K Ultra HD', cats: '6,26' },
  { key: 'bluray', title: 'Blu-ray', cats: '20' },
  { key: 'sd', title: 'SD & DVD', cats: '1,2,3' },
  { key: 'series', title: 'TV series', cats: '21,27' },
  { key: 'kdrama', title: 'K-Drama', cats: '31' },
  { key: 'anime', title: 'Anime', cats: '24' },
  { key: 'cartoons', title: 'Cartoons', cats: '15' },
  { key: 'docs', title: 'Documentaries', cats: '16' },
  { key: 'rodub', title: 'Dubbed in Romanian', cats: '28' },
];
const rowState = new Map(); // key -> { def, el, track, page, hasMore, loading, ids }

function buildRows() {
  const box = $('#rows');
  box.innerHTML = '';
  const io = new IntersectionObserver(
    (entries) => {
      for (const e of entries) {
        if (!e.isIntersecting) continue;
        io.unobserve(e.target);
        loadRow(rowState.get(e.target.dataset.key));
      }
    },
    { rootMargin: '600px 0px' },
  );
  for (const def of ROWS) {
    const el = document.createElement('section');
    el.className = 'row';
    el.dataset.key = def.key;
    el.innerHTML = `
      <div class="row-head"><h2 class="row-title">${esc(def.title)}</h2><span class="row-sub"></span></div>
      <div class="row-wrap">
        <button class="row-arrow left" type="button" tabindex="-1" aria-label="Scroll left"><svg viewBox="0 0 24 24"><path d="M15 5l-7 7 7 7"/></svg></button>
        <div class="row-track"></div>
        <button class="row-arrow right" type="button" tabindex="-1" aria-label="Scroll right"><svg viewBox="0 0 24 24"><path d="M9 5l7 7-7 7"/></svg></button>
      </div>`;
    box.append(el);
    const track = el.querySelector('.row-track');
    const row = { def, el, track, page: 0, hasMore: true, loading: false, ids: new Set(), cards: new Map(), signature: '' };
    rowState.set(def.key, row);
    el.querySelector('.left').addEventListener('click', () => track.scrollBy({ left: -track.clientWidth * 0.85 }));
    el.querySelector('.right').addEventListener('click', () => track.scrollBy({ left: track.clientWidth * 0.85 }));
    if (def.cats) {
      track.append(...skeletons(8));
      track.addEventListener('scroll', () => nearEnd(track) && loadRow(row), { passive: true });
      track.addEventListener('focusin', () => nearEnd(track) && loadRow(row));
      io.observe(el);
    } else {
      el.hidden = true; // local rows appear once they have something to show
    }
  }
}

const nearEnd = (track) => track.scrollLeft + track.clientWidth > track.scrollWidth - track.clientWidth * 0.8;

async function loadRow(row) {
  if (!row || row.loading || !row.hasMore) return;
  row.loading = true;
  try {
    const params = new URLSearchParams({ page: row.page, cats: row.def.cats });
    const data = await api(`/api/movies?${params}`);
    if (row.page === 0) row.track.innerHTML = '';
    const fresh = data.items.filter((m) => !row.ids.has(m.id));
    for (const m of fresh) row.ids.add(m.id);
    appendGrouped(row.track, row.cards, fresh);
    row.hasMore = data.hasMore && data.items.length > 0;
    row.page += 1;
    if (!row.ids.size) row.track.innerHTML = '<div class="row-empty">Nothing here right now.</div>';
    if (row.def.key === 'hd' || !heroState.started) startHero(fresh);
  } catch (err) {
    row.hasMore = false;
    if (row.page === 0) {
      row.track.innerHTML = `<div class="row-error">${esc(err.message)}</div>`;
      if (!heroState.started) hideHero();
    }
  } finally {
    row.loading = false;
  }
}

/** Re-render "Continue watching" and "My downloads" when the library or watch progress changes. */
function renderLocalRows() {
  const downloads = [...state.downloads.values()];
  const cont = downloads
    .filter((d) => resumePoint(d.id) && d.videos?.length)
    .sort((a, b) => state.progress[b.id].updatedAt - state.progress[a.id].updatedAt);
  fillLocalRow('continue', cont, (d) => {
    const pr = state.progress[d.id];
    return pr.duration ? pr.t / pr.duration : 0;
  });
  fillLocalRow('library', downloads, null);
}

function fillLocalRow(key, items, watchedOf) {
  const row = rowState.get(key);
  if (!row) return;
  const signature = items.map((d) => `${d.id}:${watchedOf ? Math.round(watchedOf(d) * 100) : ''}`).join(',');
  row.el.hidden = !items.length;
  if (signature === row.signature) return;
  row.signature = signature;
  const focusedId = row.track.contains(document.activeElement) ? document.activeElement.dataset.id : null;
  row.track.replaceChildren(...items.map((d) => card(libraryToMovie(d), { watched: watchedOf ? watchedOf(d) : null })));
  row.el.querySelector('.row-sub').textContent = key === 'library' ? `${items.length} title${items.length === 1 ? '' : 's'}` : '';
  if (focusedId) row.track.querySelector(`[data-id="${CSS.escape(focusedId)}"]`)?.focus();
}

// ---------- hero ----------
const heroState = { started: false, items: [], index: 0, timer: null };

async function startHero(candidates) {
  if (heroState.started) return;
  const seen = new Set();
  const pool = candidates
    .filter((m) => m.seeders != null || m.imdb)
    .sort((a, b) => (b.seeders || 0) - (a.seeders || 0))
    .filter((m) => !seen.has(groupKey(m)) && seen.add(groupKey(m)))
    .slice(0, 8);
  if (!pool.length) return;
  heroState.started = true;
  const metas = await Promise.all(
    pool.map((m) => api(`/api/meta?${new URLSearchParams({ imdb: m.imdb || '', name: m.name })}`).catch(() => ({}))),
  );
  heroState.items = pool.map((m, i) => ({ m, meta: metas[i] })).filter((x) => x.meta.poster).slice(0, 5);
  if (!heroState.items.length) return hideHero();
  $('#heroDots').innerHTML = heroState.items
    .map((_, i) => `<button type="button" tabindex="-1" aria-label="Featured ${i + 1}" data-i="${i}"></button>`)
    .join('');
  $('#hero').classList.remove('is-loading');
  showHero(0, true);
  heroState.timer = setInterval(() => {
    const hero = $('#hero');
    const busy = hero.contains(document.activeElement) || hero.matches(':hover') || document.hidden || $('#homeView').hidden;
    if (!busy) showHero((heroState.index + 1) % heroState.items.length);
  }, 9000);
}

function hideHero() {
  $('#hero').hidden = true;
  document.body.classList.add('no-hero');
}

function showHero(i, instant = false) {
  heroState.index = i;
  const hero = $('#hero');
  const apply = () => {
    const { m, meta } = heroState.items[i];
    const parsed = parseReleaseName(m.name);
    const year = meta.year || parsed.year;
    $('#heroKicker').textContent = i === 0 ? 'Most seeded new release' : 'Popular on FileList';
    $('#heroTitle').textContent = meta.title || parsed.title;
    const tags = releaseTags(m.name, m.category).slice(0, 4);
    $('#heroMeta').innerHTML = [
      m.seeders != null ? `<span class="match">${m.seeders} seeders</span>` : '',
      year ? `<span>${year}</span>` : '',
      ...tags.map((t) => `<span class="tag ${t === '4K' ? 'red' : ''}">${esc(t)}</span>`),
      m.size ? `<span>${esc(m.size)}</span>` : '',
    ].join('');
    $('#heroStars').textContent = meta.stars ? `Starring ${meta.stars}` : '';
    $('#heroImg').style.backgroundImage = `url("${meta.posterLarge}")`;
    $('#heroPoster').src = meta.posterLarge;
    updateHeroAction();
    document.querySelectorAll('#heroDots button').forEach((b, j) => b.classList.toggle('active', j === i));
  };
  if (instant) return apply();
  hero.classList.add('swap');
  setTimeout(() => {
    apply();
    hero.classList.remove('swap');
  }, 450);
}

function updateHeroAction() {
  const item = heroState.items[heroState.index];
  if (!item) return;
  const btn = $('#heroPrimary');
  const dl = state.downloads.get(item.m.id);
  if (dl?.checking) {
    btn.innerHTML = `${ICON_DOWN} Verifying files…`;
    btn.onclick = () => openDetails(item.m.id);
  } else if (dl?.done && dl.videos?.length) {
    const pr = resumePoint(item.m.id);
    btn.innerHTML = `${ICON_PLAY} ${pr ? 'Resume' : 'Play'}`;
    btn.onclick = () => openPlayer(item.m.id);
  } else if (dl) {
    btn.innerHTML = `${ICON_DOWN} Downloading ${pct(dl.progress)}`;
    btn.onclick = () => openDetails(item.m.id);
  } else {
    btn.innerHTML = `${ICON_DOWN} Download`;
    btn.onclick = () => startDownload(item.m.id);
  }
}

$('#heroInfo').addEventListener('click', () => {
  const item = heroState.items[heroState.index];
  if (item) openDetails(item.m.id);
});
$('#heroDots').addEventListener('click', (e) => {
  const b = e.target.closest('[data-i]');
  if (b) showHero(Number(b.dataset.i));
});

// ---------- views & search ----------
function showView(view) {
  $('#homeView').hidden = view !== 'home';
  $('#searchView').hidden = view !== 'search';
  document.querySelector('.nav-link[data-view="home"]').classList.toggle('active', view === 'home');
  updateNav();
  if (view === 'home') window.scrollTo({ top: 0 });
}

function updateNav() {
  $('#nav').classList.toggle('solid', window.scrollY > 40 || !$('#searchView').hidden);
}
window.addEventListener('scroll', updateNav, { passive: true });

let generation = 0;
async function loadMovies(reset = false) {
  const s = state.search;
  if (reset) {
    generation += 1;
    Object.assign(s, { loading: false, page: 0, hasMore: true, emptyPages: 0 });
    $('#grid').replaceChildren(...skeletons(12));
  }
  if (s.loading || !s.hasMore) return;
  const gen = generation;
  s.loading = true;
  $('#sentinel').textContent = reset ? '' : 'Loading…';
  $('#searchTitle').innerHTML = s.query ? `Results for <b>“${esc(s.query)}”</b>` : 'All videos';
  try {
    const data = await api(`/api/movies?${new URLSearchParams({ page: s.page, q: s.query, cats: s.cats })}`);
    if (gen !== generation) return; // a newer search replaced this one
    if (s.page === 0) {
      $('#grid').innerHTML = '';
      s.cards = new Map();
    }
    appendGrouped($('#grid'), s.cards, data.items);
    // FileList's search also matches games, apps, music…, which the server drops. Don't page through
    // endless non-movie results: give up after a few pages in a row without a single movie.
    s.emptyPages = data.items.length ? 0 : (s.emptyPages || 0) + 1;
    s.hasMore = data.hasMore && s.emptyPages < 3;
    s.page += 1;
    const count = $('#grid').children.length;
    $('#status').textContent = count
      ? `${count} titles`
      : s.hasMore
        ? 'Searching…'
        : 'Nothing found. Try another title or an IMDb id.';
    $('#status').classList.remove('error');
  } catch (err) {
    if (gen !== generation) return;
    $('#grid').innerHTML = '';
    $('#status').textContent = err.message;
    $('#status').classList.add('error');
    s.hasMore = false;
  } finally {
    if (gen === generation) {
      s.loading = false;
      $('#sentinel').textContent = s.hasMore || !$('#grid').children.length ? '' : 'End of results';
    }
  }
  if (gen === generation && s.hasMore && $('#sentinel').getBoundingClientRect().top < window.innerHeight + 400) loadMovies();
}

function runSearch(q) {
  state.search.query = q;
  if (!q) return showView('home');
  showView('search');
  loadMovies(true);
}

let searchTimer;
$('#searchToggle').addEventListener('click', () => {
  const form = $('#searchForm');
  form.classList.add('open');
  $('#searchInput').focus();
});
$('#searchInput').addEventListener('blur', () => {
  if (!$('#searchInput').value) $('#searchForm').classList.remove('open');
});
$('#searchForm').addEventListener('submit', (e) => {
  e.preventDefault();
  clearTimeout(searchTimer);
  runSearch($('#searchInput').value.trim());
});
// Every search costs FileList page views (the account has an hourly limit): search as you type only once the
// typing pauses and the text is long enough. Enter searches right away.
$('#searchInput').addEventListener('input', () => {
  clearTimeout(searchTimer);
  searchTimer = setTimeout(() => {
    const q = $('#searchInput').value.trim();
    if (q !== state.search.query && (q.length >= 3 || !q)) runSearch(q);
  }, 1200);
});
$('#categorySelect').addEventListener('change', (e) => {
  state.search.cats = e.target.value;
  loadMovies(true);
});
new IntersectionObserver((entries) => {
  if (entries[0].isIntersecting && !$('#searchView').hidden) loadMovies();
}, { rootMargin: '400px' }).observe($('#sentinel'));

function goHome() {
  $('#searchInput').value = '';
  $('#searchForm').classList.remove('open');
  state.search.query = '';
  showView('home');
}
$('#logo').addEventListener('click', goHome);
document.querySelector('.nav-link[data-view="home"]').addEventListener('click', goHome);

// ---------- details ----------
function openDetails(id) {
  const dl = state.downloads.get(id);
  const m = state.movies.get(id) || (dl && libraryToMovie(dl));
  if (!m) return;
  if (!state.movies.has(id)) rememberMovie(m);
  const { title } = parseReleaseName(m.name);
  $('#detailsTitle').textContent = title;
  $('#detailsPoster').src = posterUrl(m);
  $('#detailsPoster').onerror = () => ($('#detailsPoster').src = fallbackPoster(m));
  $('#detailsBg').style.backgroundImage = `url("${posterUrl(m)}")`;
  $('#detailsStars').textContent = '–';
  const imdb = $('#detailsImdb');
  imdb.classList.toggle('hidden', !m.imdb);
  if (m.imdb) imdb.href = `https://www.imdb.com/title/${m.imdb}/`;
  selectRelease(id);
  $('#detailsDialog').showModal();
  $('#detailsAction').focus();

  api(`/api/meta?${new URLSearchParams({ imdb: m.imdb || '', name: m.name })}`)
    .then((meta) => {
      if (state.detailsId !== id || !meta.poster) return;
      $('#detailsTitle').textContent = meta.title || title;
      if (meta.stars) $('#detailsStars').textContent = meta.stars;
      $('#detailsBg').style.backgroundImage = `url("${meta.posterLarge}")`;
      if (!m.poster) $('#detailsPoster').src = meta.posterLarge;
      if (!m.imdb && meta.imdb) {
        imdb.classList.remove('hidden');
        imdb.href = `https://www.imdb.com/title/${meta.imdb}/`;
      }
    })
    .catch(() => {});
}

/** Show one release of the movie in the details view (the main button acts on it). */
function selectRelease(id) {
  const m = state.movies.get(id);
  if (!m) return;
  state.detailsId = id;
  const { year } = parseReleaseName(m.name);
  $('#detailsMeta').innerHTML = [
    m.seeders != null ? `<span class="match">${m.seeders} seeders</span>` : '',
    year ? `<span>${year}</span>` : '',
    m.size ? `<span>${esc(m.size)}</span>` : '',
    m.freeleech ? '<span class="tag green">FREELEECH</span>' : '',
  ].join('');
  $('#detailsTags').innerHTML = releaseTags(m.name, m.category)
    .map((t) => `<span class="tag ${t === '4K' ? 'red' : ''}">${esc(t)}</span>`)
    .join('');
  $('#detailsDesc').textContent = m.description || '';
  $('#detailsCategory').textContent = m.category || '–';
  $('#detailsSwarm').textContent = m.seeders != null ? `${m.seeders} seeding · ${m.leechers ?? 0} downloading` : '–';
  $('#detailsAdded').textContent = m.added || '–';
  $('#detailsRelease').textContent = m.name;
  $('#detailsNote').textContent = '';
  $('#detailsNote').classList.remove('error');
  updateDetailsAction();
}

function renderVersions() {
  const id = state.detailsId;
  const releases = id ? releasesOf(id) : [];
  $('#detailsVersionsBox').classList.toggle('hidden', releases.length < 2);
  if (releases.length < 2) return;
  const focusedId = document.activeElement?.closest?.('.version')?.dataset.id;
  $('#detailsVersions').innerHTML = releases
    .map((r) => {
      const dl = state.downloads.get(r.id);
      const tags = releaseTags(r.name, r.category).slice(0, 5);
      const status = dl ? (dl.done ? '✓ Downloaded' : `⬇ ${pct(dl.progress)}`) : '';
      return `
        <button type="button" class="version ${r.id === id ? 'selected' : ''}" data-id="${esc(r.id)}" title="${esc(r.name)}">
          <span class="v-tags">${tags.map((t) => `<span class="tag ${t === '4K' ? 'red' : ''}">${esc(t)}</span>`).join('') || '<span class="tag">SD</span>'}${
            r.freeleech ? '<span class="tag green">FREE</span>' : ''
          }</span>
          <span class="v-size">${esc(r.size || '')}</span>
          <span class="v-seed">${r.seeders != null ? `▲ ${r.seeders}` : ''}</span>
          <span class="v-state ${dl?.done ? 'owned' : ''}">${status}</span>
        </button>`;
    })
    .join('');
  if (focusedId) $(`#detailsVersions .version[data-id="${CSS.escape(focusedId)}"]`)?.focus({ preventScroll: true });
}

$('#detailsVersions').addEventListener('click', (e) => {
  const v = e.target.closest('.version');
  if (v) selectRelease(v.dataset.id);
});

function updateDetailsAction() {
  const id = state.detailsId;
  if (!id) return;
  renderVersions();
  const btn = $('#detailsAction');
  const dl = state.downloads.get(id);
  const bar = $('#detailsProgress');
  bar.classList.toggle('hidden', !dl);
  btn.disabled = false;
  if (!dl) {
    btn.innerHTML = `${ICON_DOWN} Download`;
    btn.onclick = () => startDownload(id);
    return;
  }
  bar.classList.toggle('done', dl.done);
  bar.firstElementChild.style.width = pct(dl.progress);
  const canPlay = dl.done && dl.videos?.length > 0 && !dl.checking;
  const pr = resumePoint(id);
  if (dl.checking) btn.textContent = 'Verifying files…';
  else if (canPlay && pr) btn.innerHTML = `${ICON_PLAY} Resume from ${clock(pr.t)}`;
  else if (canPlay) btn.innerHTML = `${ICON_PLAY} Play`;
  else if (!dl.ready) btn.textContent = 'Connecting…';
  else if (!dl.videos?.length) btn.textContent = 'No video file in torrent';
  else btn.innerHTML = `${ICON_DOWN} Downloading ${pct(dl.progress)}`;
  btn.disabled = !canPlay;
  btn.onclick = () => {
    $('#detailsDialog').close();
    openPlayer(id);
  };
  const note = $('#detailsNote');
  note.classList.toggle('error', Boolean(dl.error));
  note.textContent = dl.connecting
    ? 'Connecting to qBittorrent…'
    : dl.error
    ? dl.error
    : dl.checking
      ? `qBittorrent is verifying the downloaded files… ${pct(dl.checkProgress)}`
      : dl.done
      ? 'Downloaded · seeding while the app runs (good for your ratio)'
      : dl.held
      ? `${pct(dl.progress)} downloaded · paused while a video is playing`
      : `${pct(dl.progress)} downloaded · ${bytes(dl.downloadSpeed)}/s · ${dl.peers} peers${dl.timeRemaining ? ` · ${eta(dl.timeRemaining)} left` : ''}`;
}

async function startDownload(id) {
  const m = state.movies.get(id);
  const inDetails = state.detailsId === id;
  if (inDetails) {
    $('#detailsAction').disabled = true;
    $('#detailsAction').textContent = 'Fetching torrent…';
  }
  try {
    const status = await api(`/api/downloads/${id}`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ name: m?.name, imdb: m?.imdb, poster: m?.poster }),
    });
    state.downloads.set(id, status);
    toast(`Downloading ${parseReleaseName(status.name || m?.name || '').title}`);
    onDownloadsChanged();
  } catch (err) {
    if (inDetails) {
      $('#detailsNote').textContent = err.message;
      $('#detailsNote').classList.add('error');
    } else {
      toast(err.message, true);
    }
  } finally {
    if (inDetails) updateDetailsAction();
  }
}

// ---------- downloads ----------
function onDownloadsChanged() {
  $('#downloadsCount').textContent = state.downloads.size;
  renderDownloads();
  updateDetailsAction();
  updateHeroAction();
  updateAllCards();
  renderLocalRows();
}

async function pollDownloads() {
  try {
    const res = await fetch('/api/downloads');
    // The server was updated/restarted: reload to get the new UI (TV browsers otherwise keep the old one).
    const build = res.headers.get('X-Build');
    if (state.build && build && build !== state.build && !state.player) return location.reload();
    state.build = build || state.build;
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    const list = await res.json();
    state.downloads = new Map(list.map((d) => [d.id, d]));
    onDownloadsChanged();
  } catch {
    /* server restarting */
  }
  setTimeout(pollDownloads, 2000);
}

function renderDownloads() {
  const box = $('#downloadsList');
  if (!$('#drawer').classList.contains('open')) return;
  if (!state.downloads.size) {
    box.innerHTML = '<div class="empty">Nothing downloaded yet.<br>Pick a movie and press Download.</div>';
    return;
  }
  const focused = document.activeElement?.closest('.dl-item') && document.activeElement.dataset.act;
  const focusedId = focused && document.activeElement.closest('.dl-item').dataset.id;
  box.innerHTML = [...state.downloads.values()]
    .map((d) => {
      const stats = d.connecting
        ? 'Connecting to qBittorrent…'
        : d.error
        ? `<span class="err">${esc(d.error)}</span>`
        : d.checking
          ? `Verifying downloaded files… ${pct(d.checkProgress)}`
          : d.done
          ? `Done · ${bytes(d.length)} · seeding ↑ ${bytes(d.uploadSpeed)}/s · ratio ${(d.ratio || 0).toFixed(2)}`
          : d.held
          ? `${pct(d.progress)} of ${bytes(d.length)} · paused while a video is playing`
          : `${pct(d.progress)} of ${bytes(d.length)} · ↓ ${bytes(d.downloadSpeed)}/s · ${d.peers} peers${d.timeRemaining ? ` · ${eta(d.timeRemaining)}` : ''}`;
      return `
      <div class="dl-item" data-id="${esc(d.id)}">
        <img src="${esc(posterUrl(d))}" alt="" />
        <div class="dl-info">
          <div class="dl-name" title="${esc(d.name)}">${esc(parseReleaseName(d.name).title)}</div>
          <div class="progress ${d.done ? 'done' : ''}"><div style="width:${pct(d.progress)}"></div></div>
          <div class="dl-stats">${stats}</div>
          <div class="dl-actions">
            <button class="btn btn-light small" data-act="play" ${d.done && d.videos?.length && !d.checking ? '' : 'disabled'}>${ICON_PLAY} ${resumePoint(d.id) ? 'Resume' : 'Play'}</button>
            <button class="btn btn-glass small" data-act="remove">Remove</button>
            <button class="btn btn-glass small danger" data-act="delete">Delete files</button>
          </div>
        </div>
      </div>`;
    })
    .join('');
  if (focusedId) box.querySelector(`.dl-item[data-id="${CSS.escape(focusedId)}"] [data-act="${focused}"]`)?.focus();
}

$('#downloadsList').addEventListener('click', async (e) => {
  const btn = e.target.closest('button[data-act]');
  if (!btn) return;
  const id = btn.closest('.dl-item').dataset.id;
  const act = btn.dataset.act;
  if (act === 'play') {
    toggleDrawer(false);
    return openPlayer(id);
  }
  const name = state.downloads.get(id)?.name;
  const msg =
    act === 'delete'
      ? `Remove "${name}" and DELETE the downloaded files from disk?`
      : `Remove "${name}" from qBittorrent? Files on disk are kept.`;
  if (!confirm(msg)) return;
  await api(`/api/downloads/${id}?deleteFiles=${act === 'delete' ? 1 : 0}`, { method: 'DELETE' }).catch((err) =>
    toast(err.message, true),
  );
  state.downloads.delete(id);
  delete state.progress[id];
  onDownloadsChanged();
});

function toggleDrawer(open) {
  $('#drawer').classList.toggle('open', open);
  $('#drawer').setAttribute('aria-hidden', String(!open));
  $('#scrim').classList.toggle('show', open);
  document.body.classList.toggle('no-scroll', open);
  renderDownloads();
  if (open) setTimeout(() => $('#downloadsList button:not([disabled])')?.focus() || $('#drawerClose').focus(), 50);
}
$('#navDownloads').addEventListener('click', () => toggleDrawer(!$('#drawer').classList.contains('open')));
$('#drawerClose').addEventListener('click', () => toggleDrawer(false));
$('#scrim').addEventListener('click', () => toggleDrawer(false));

// ---------- watch progress ----------
async function loadProgress() {
  state.progress = await api('/api/progress').catch(() => state.progress);
  renderLocalRows();
}

let lastSaved = 0;
function saveProgress(force = false) {
  const p = state.player;
  if (!p || p.pendingSeek != null) return;
  if (!force && Date.now() - lastSaved < 10000) return;
  const t = currentPosition(p);
  if (t < 5) return;
  lastSaved = Date.now();
  const duration = p.compat ? p.duration : Number.isFinite(video.duration) ? video.duration : p.duration;
  const entry = { t, duration, index: p.index };
  state.progress[p.id] = { ...entry, updatedAt: Date.now() };
  fetch(`/api/progress/${p.id}`, {
    method: 'PUT',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(entry),
    keepalive: true,
  }).catch(() => {});
}

// ---------- player ----------
const video = $('#video');

// Downloads stop while a video plays on any device: tell the server this one is still playing.
function reportPlaying() {
  if (!state.player || video.paused || video.ended) return;
  fetch('/api/playing', { method: 'POST', keepalive: true }).catch(() => {});
}
setInterval(reportPlaying, 10000);
video.addEventListener('playing', reportPlaying);

function openPlayer(id) {
  const d = state.downloads.get(id);
  if (!d?.videos?.length) return;
  if (d.checking) return toast('qBittorrent is still verifying this movie. It can be played once that finishes.');
  if (!d.done) return toast('This movie can be played once the download finishes.');
  const pr = resumePoint(id);
  const index = d.videos.some((v) => v.index === pr?.index) ? pr.index : d.videos[0].index;
  state.player = {
    id,
    index,
    quality: 'original',
    compat: false,
    offset: 0,
    duration: pr?.duration || null,
    resumeAt: pr ? pr.t : 0,
    pendingSeek: null,
    subKey: null,
    subDelay: 0,
    tracks: [],
  };
  $('#playerTitle').textContent = parseReleaseName(d.name).title || d.name;
  $('#fileSelect').innerHTML = d.videos
    .map((v) => `<option value="${v.index}">${esc(v.name)} (${bytes(v.length)})</option>`)
    .join('');
  $('#fileSelect').value = String(index);
  $('#fileLabel').classList.toggle('hidden', d.videos.length < 2);
  $('#subsSelect').innerHTML = '<option value="">Off</option>';
  $('#startOver').classList.toggle('hidden', !pr);
  updateSubControls();
  setSubNote('');
  $('#qualityLabel').classList.toggle('hidden', !state.config?.ffmpeg);
  $('#playerDialog').showModal();
  video.focus();
  reportPlayer('open', reportPlayer.capabilities());
  refreshSubtitles();
  startPlayback();
  if (pr) showOsd(`Resuming at ${clock(pr.t)}`);
}

$('#startOver').addEventListener('click', () => {
  const p = state.player;
  if (!p) return;
  p.resumeAt = 0;
  $('#startOver').classList.add('hidden');
  applyQuality(p.quality, 0);
  video.focus();
});
$('#settingsToggle').addEventListener('click', () => $('#settingsPanel').classList.toggle('hidden'));

let osdTimer;
function showOsd(text) {
  const el = $('#osd');
  el.textContent = text;
  el.classList.add('show');
  clearTimeout(osdTimer);
  osdTimer = setTimeout(() => el.classList.remove('show'), 1600);
}

// ---------- quality ----------
// 'original' = the file as-is, 'remux' = original quality converted by ffmpeg (unsupported formats),
// anything else = a reduced-bitrate preset from the server (for slow Wi-Fi).
const QUALITY_LABELS = { auto: 'Auto', original: 'Original', remux: 'Original (converted)' };
const qualityLabel = (q) => {
  const preset = state.config?.quality?.[q];
  return preset ? `${preset.height}p · ${preset.mbps} Mbit/s` : QUALITY_LABELS[q] || q;
};

function fillQualitySelect() {
  const presets = Object.keys(state.config?.quality || {});
  $('#qualitySelect').innerHTML = ['auto', 'original', 'remux', ...presets]
    .map((q) => `<option value="${q}">${esc(qualityLabel(q))}</option>`)
    .join('');
}

// What this device can decode. Sent to the server, which then only converts what it must (often just the audio).
const DEVICE_CAPS = (() => {
  const supported = (type) => {
    try {
      if (window.MediaSource && MediaSource.isTypeSupported(type)) return true;
    } catch {
      /* fall through */
    }
    return Boolean(video.canPlayType(type));
  };
  const pick = (list) => [...new Set(list.filter(([, type]) => supported(type)).map(([codec]) => codec))];
  return {
    video: pick([
      ['h264', 'video/mp4; codecs="avc1.640028"'],
      ['hevc', 'video/mp4; codecs="hvc1.2.4.L150.B0"'],
      ['hevc', 'video/mp4; codecs="hev1.2.4.L150.B0"'],
      ['av1', 'video/mp4; codecs="av01.0.08M.08"'],
      ['vp9', 'video/mp4; codecs="vp09.00.40.08"'],
    ]),
    audio: pick([
      ['aac', 'audio/mp4; codecs="mp4a.40.2"'],
      ['mp3', 'audio/mp4; codecs="mp4a.6B"'],
      ['ac3', 'audio/mp4; codecs="ac-3"'],
      ['eac3', 'audio/mp4; codecs="ec-3"'],
      ['flac', 'audio/mp4; codecs="fLaC"'],
      ['opus', 'audio/mp4; codecs="opus"'],
    ]),
  };
})();
const capsQuery = () => `vc=${DEVICE_CAPS.video.join(',')}&ac=${DEVICE_CAPS.audio.join(',')}`;

/** Download a test payload for up to `ms` and return the throughput in Mbit/s. */
async function measureMbps(ms = 3000) {
  const ctrl = new AbortController();
  const timer = setTimeout(() => ctrl.abort(), ms);
  const t0 = performance.now();
  let received = 0;
  try {
    const res = await fetch(`/api/speedtest?bytes=40000000&r=${Math.random()}`, { signal: ctrl.signal, cache: 'no-store' });
    const reader = res.body.getReader();
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      received += value.length;
    }
  } catch {
    /* aborted after `ms`: that's the point */
  } finally {
    clearTimeout(timer);
  }
  const secs = (performance.now() - t0) / 1000;
  return received ? (received * 8) / 1e6 / secs : null;
}

/** This device's connection speed. Measured at most every 15 minutes (each measurement costs ~4 s). */
async function connectionMbps() {
  try {
    const cached = JSON.parse(store.get('speed') || 'null');
    if (cached && Date.now() - cached.at < 15 * 60 * 1000) return cached.mbps;
  } catch {
    /* no cache */
  }
  $('#playerNote').textContent = 'Measuring the connection speed…';
  // The first request makes the server stop the downloads (playback is about to start); give them a moment
  // before measuring, or a busy Wi-Fi makes the link look much slower than it is during playback.
  await measureMbps(800);
  await new Promise((r) => setTimeout(r, 2500));
  const mbps = await measureMbps();
  if (mbps) store.set('speed', JSON.stringify({ mbps, at: Date.now() }));
  return mbps;
}

/**
 * Pick how to play: the original file if this device can decode it and the connection carries it; converted
 * audio (video copied untouched) if only the audio is the problem; otherwise the best reduced quality that fits.
 */
async function autoQuality(p) {
  $('#playerNote').classList.remove('error');
  const [mbps, info] = await Promise.all([connectionMbps(), api(`/api/probe/${p.id}/${p.index}`).catch(() => ({}))]);
  if (info.duration) p.duration = info.duration;
  const needed = info.bitrate ? info.bitrate / 1e6 : null;
  const fits = !mbps || (needed ? mbps * 0.75 >= needed : mbps >= 30);
  const videoOk = !info.videoCodec || DEVICE_CAPS.video.includes(info.videoCodec);
  const audioCodec = info.audio?.[0]?.codec;
  const audioOk = !audioCodec || DEVICE_CAPS.audio.includes(audioCodec) || ['vorbis', 'opus'].includes(audioCodec);
  const presets = Object.entries(state.config.quality).sort((a, b) => b[1].mbps - a[1].mbps);
  const bestPreset = () => (presets.find(([, pr]) => !mbps || pr.mbps <= mbps * 0.6) || presets[presets.length - 1])[0];
  let q;
  if (fits && videoOk) q = audioOk ? 'original' : 'remux';
  else q = bestPreset();
  const why = !videoOk ? ` (this device can't decode ${info.videoCodec})` : !audioOk && q === 'remux' ? ` (converting ${audioCodec} audio)` : '';
  p.autoNote = mbps
    ? `Connection ≈ ${mbps.toFixed(1)} Mbit/s${needed ? `, this file needs ≈ ${needed.toFixed(1)}` : ''} → ${qualityLabel(q)}${why}`
    : '';
  reportPlayer('auto-quality', { mbps, needed, chosen: q, caps: DEVICE_CAPS, videoCodec: info.videoCodec, audioCodec });
  return q;
}

async function startPlayback() {
  const p = state.player;
  fillQualitySelect();
  const pref = state.config?.ffmpeg ? store.get('quality') || 'auto' : 'original';
  $('#qualitySelect').value = pref;
  const q = pref === 'auto' ? await autoQuality(p) : pref;
  if (state.player === p) applyQuality(q, p.resumeAt || 0);
}

const currentPosition = (p) =>
  p.pendingSeek != null ? p.pendingSeek : Math.floor((p.compat ? p.offset : 0) + (video.currentTime || 0));

/**
 * (Re)start playback at `startAt` seconds with the given quality. Three modes:
 *   direct  — the original file (byte ranges; native seeking)
 *   hls     — reduced quality as HLS (native seeking; segments produced just ahead of playback)
 *   convert — full quality converted as one stream (seeking restarts it: `offset` + our own seek bar)
 */
function applyQuality(q, startAt) {
  const p = state.player;
  if (!p) return;
  p.quality = q;
  p.mode = q === 'original' ? 'direct' : state.config?.quality?.[q] ? 'hls' : 'convert';
  p.compat = p.mode === 'convert';
  p.offset = p.compat ? Math.floor(startAt) : 0;
  p.pendingSeek = !p.compat && startAt > 0 ? startAt : null; // applied once metadata is known
  $('#seekRange').value = p.offset;
  loadVideo();
}

let hls = null;
function destroyHls() {
  if (hls) {
    hls.destroy();
    hls = null;
  }
}

$('#qualitySelect').addEventListener('change', async (e) => {
  const p = state.player;
  if (!p) return;
  store.set('quality', e.target.value);
  p.autoNote = '';
  const position = currentPosition(p);
  const q = e.target.value === 'auto' ? await autoQuality(p) : e.target.value;
  if (state.player === p) applyQuality(q, position);
});

function loadVideo() {
  const p = state.player;
  $('#playerNote').classList.remove('error');
  $('#playerNote').textContent = p.autoNote || '';
  $('#seekBar').classList.toggle('hidden', !p.compat);
  destroyHls();
  if (p.mode === 'hls') {
    const url = `/hls/${p.id}/${p.index}/${p.quality}/index.m3u8?audio=0`;
    if (window.Hls && Hls.isSupported()) {
      // start where we resume, so the server doesn't first convert the beginning
      hls = new Hls({ startPosition: p.pendingSeek ?? -1, maxBufferLength: 30, maxMaxBufferLength: 60 });
      p.pendingSeek = null;
      let recovered = false;
      hls.on(Hls.Events.ERROR, (_e, data) => {
        if (!data.fatal) return;
        reportPlayer('hls-error', { type: data.type, details: data.details });
        if (data.type === Hls.ErrorTypes.MEDIA_ERROR && !recovered) {
          recovered = true;
          hls.recoverMediaError();
          return;
        }
        $('#playerNote').classList.add('error');
        $('#playerNote').textContent = `Playback failed (${data.details}). Try another Quality setting.`;
      });
      hls.loadSource(url);
      hls.attachMedia(video);
    } else {
      video.src = url; // native HLS (e.g. TV browsers, Safari)
    }
  } else {
    video.src = p.compat ? `/transcode/${p.id}/${p.index}?t=${p.offset}&${capsQuery()}` : `/stream/${p.id}/${p.index}`;
  }
  loadSubtitles();
  video.play().catch(() => {});
  if (p.compat && p.duration == null) {
    api(`/api/probe/${p.id}/${p.index}`)
      .then((info) => {
        p.duration = info.duration;
        $('#seekRange').max = Math.floor(info.duration || 0);
      })
      .catch(() => {});
  } else if (p.compat) {
    $('#seekRange').max = Math.floor(p.duration || 0);
  }
}

/** Seek relative to the current position (remote ←/→). Converted streams restart ffmpeg, so presses are batched. */
let seekTimer;
let seekTarget = null;
function seekBy(delta) {
  const p = state.player;
  if (!p) return;
  const duration = p.compat ? p.duration : video.duration;
  const base = seekTarget ?? currentPosition(p);
  seekTarget = Math.max(0, Math.min(Number.isFinite(duration) && duration ? duration - 5 : Infinity, base + delta));
  showOsd(`${delta > 0 ? '⏩' : '⏪'} ${clock(seekTarget)}${duration ? ` / ${clock(duration)}` : ''}`);
  clearTimeout(seekTimer);
  seekTimer = setTimeout(
    () => {
      const target = seekTarget;
      seekTarget = null;
      if (p.compat) applyQuality(p.quality, target);
      else video.currentTime = target;
    },
    p.compat ? 700 : 0,
  );
}

function togglePlay() {
  if (video.paused) video.play().catch(() => {});
  else video.pause();
  showOsd(video.paused ? '❚❚ Paused' : '▶ Playing');
}

// ---------- subtitles ----------
function setSubNote(text, isError = false) {
  $('#subsNote').textContent = text;
  $('#subsNote').classList.toggle('error', isError);
}

/** Reload the list of available subtitles and pick one (explicit choice > last choice > Romanian > off). */
async function refreshSubtitles(preferKey) {
  const p = state.player;
  if (!p) return;
  const tracks = await api(`/api/subs/${p.id}?video=${p.index}`).catch(() => []);
  if (state.player !== p) return;
  p.tracks = tracks;
  const groups = [
    ['upload', 'Your files'],
    ['torrent', 'In the torrent'],
    ['embedded', 'In the video file'],
    ['online', 'Online (OpenSubtitles)'],
  ];
  $('#subsSelect').innerHTML =
    '<option value="">Off</option>' +
    groups
      .map(([source, label]) => {
        const items = tracks.filter((t) => t.source === source);
        if (!items.length) return '';
        const options = items
          .map(
            (t) =>
              `<option value="${esc(t.key)}" ${t.ready ? '' : 'disabled'}>${esc(t.label)}${t.lang ? ` [${esc(t.lang)}]` : ''}${
                t.ready ? '' : ` (${esc(t.reason)})`
              }</option>`,
          )
          .join('');
        return `<optgroup label="${label}">${options}</optgroup>`;
      })
      .join('');

  const usable = (k) => k === '' || tracks.some((t) => t.key === k && t.ready);
  const pick =
    [preferKey, p.subKey, store.get(`sub:${p.id}`)].find((k) => k != null && usable(k)) ??
    tracks.find((t) => t.ready && t.lang === 'ro')?.key ??
    '';
  $('#subsSelect').value = pick;
  if (pick !== p.subKey) selectSubtitle(pick);
}

function selectSubtitle(key) {
  const p = state.player;
  p.subKey = key;
  store.set(`sub:${p.id}`, key);
  p.subDelay = Number(store.get(`subdelay:${p.id}:${key}`)) || 0;
  updateSubControls();
  loadSubtitles();
}

function updateSubControls() {
  const p = state.player;
  $('#subsRemove').classList.toggle('hidden', !p?.subKey?.startsWith('u:'));
  $('#subsDelayBox').classList.toggle('hidden', !p?.subKey);
  const d = p?.subDelay || 0;
  $('#subsDelay').textContent = `${d > 0 ? '+' : ''}${d.toFixed(1)}s`;
}

function clearTracks() {
  const p = state.player;
  if (p) clearTimeout(p.liveTimer);
  video.querySelectorAll('track').forEach((t) => {
    URL.revokeObjectURL(t.src);
    t.remove();
  });
  // live tracks (addTextTrack) can't be removed: empty and hide them
  for (const t of video.textTracks) {
    if (t.cues) for (const cue of [...t.cues]) t.removeCue(cue);
    t.mode = 'disabled';
  }
  if (p) p.activeTrack = null;
}

/**
 * Built-in subtitles without waiting for an extraction: the server picks the lines out of the video as it
 * streams (or out of ffmpeg's conversion), and we poll for new ones as playback moves along.
 */
function startLiveSubtitles(p, info, token) {
  const track = video.addTextTrack('subtitles', info.label || 'Subtitles', info.lang || '');
  track.mode = 'showing';
  p.activeTrack = track;
  const live = { version: 0, count: 0, offset: p.compat ? p.offset : 0 };
  setSubNote('Subtitles appear as the movie plays (read from the video while it streams).');
  const poll = async () => {
    if (state.player !== p || token !== p.subToken) return;
    try {
      const data = await api(`/api/subs/${p.id}/live?key=${encodeURIComponent(p.subKey)}&since=${live.version}`);
      if (state.player !== p || token !== p.subToken) return;
      if (data.cached && live.count === 0) {
        // the complete track was extracted meanwhile: use it
        info.cached = true;
        info.live = false;
        return loadSubtitles();
      }
      for (const [start, end, text] of data.cues) {
        const cue = new VTTCue(Math.max(0, start - live.offset), Math.max(0.1, end - live.offset), text);
        cue.origStart = cue.startTime;
        cue.origEnd = cue.endTime;
        cue.startTime = Math.max(0, cue.origStart + p.subDelay);
        cue.endTime = Math.max(0, cue.origEnd + p.subDelay);
        track.addCue(cue);
        live.count += 1;
      }
      live.version = data.version;
      if (live.count && $('#subsNote').textContent.startsWith('Subtitles appear')) setSubNote('');
    } catch {
      /* try again on the next poll */
    }
    p.liveTimer = setTimeout(poll, 2500);
  };
  poll();
}

async function loadSubtitles() {
  const p = state.player;
  if (!p) return;
  const token = (p.subToken = (p.subToken || 0) + 1);
  clearTracks();
  if (!p.subKey) return setSubNote('');
  const info = p.tracks.find((t) => t.key === p.subKey);
  if (info?.source === 'embedded' && info.live) return startLiveSubtitles(p, info, token);
  const extracting = info?.source === 'embedded' && !info.cached;
  setSubNote(extracting ? 'Extracting subtitles from the video file…' : 'Loading subtitles…');
  // Extraction reads the whole movie (minutes for big files): show its progress while we wait.
  const progressTimer = extracting
    ? setInterval(async () => {
        const tracks = await api(`/api/subs/${p.id}?video=${p.index}`).catch(() => null);
        const t = tracks?.find((x) => x.key === p.subKey);
        if (state.player !== p || token !== p.subToken || !t?.extracting) return;
        setSubNote(
          t.extracting.queued
            ? 'Subtitles: waiting for another extraction to finish…'
            : `Extracting subtitles from the video file… ${Math.round(t.extracting.progress * 100)}% (reads the whole movie once)`,
        );
      }, 3000)
    : null;
  try {
    const res = await fetch(`/subs/${p.id}/${encodeURIComponent(p.subKey)}?offset=${p.compat ? p.offset : 0}`);
    clearInterval(progressTimer);
    if (!res.ok) throw new Error((await res.json().catch(() => ({}))).error || `HTTP ${res.status}`);
    const vtt = await res.text();
    if (state.player !== p || token !== p.subToken) return;
    const track = document.createElement('track');
    track.kind = 'subtitles';
    track.label = info?.label || 'Subtitles';
    track.default = true;
    track.src = URL.createObjectURL(new Blob([vtt], { type: 'text/vtt' }));
    track.addEventListener('load', () => {
      track.track.mode = 'showing';
      for (const cue of track.track.cues) {
        cue.origStart = cue.startTime;
        cue.origEnd = cue.endTime;
      }
      applySubDelay();
    });
    video.append(track);
    track.track.mode = 'showing';
    p.activeTrack = track.track;
    setSubNote('');
  } catch (err) {
    clearInterval(progressTimer);
    if (token === p.subToken) setSubNote(`Could not load subtitles: ${err.message}`, true);
  }
}

function applySubDelay() {
  const p = state.player;
  const cues = p?.activeTrack?.cues;
  if (!p || !cues) return;
  for (const cue of cues) {
    if (cue.origStart == null) continue;
    cue.startTime = Math.max(0, cue.origStart + p.subDelay);
    cue.endTime = Math.max(0, cue.origEnd + p.subDelay);
  }
}

function changeSubDelay(delta) {
  const p = state.player;
  if (!p?.subKey) return;
  p.subDelay = Math.round((p.subDelay + delta) * 10) / 10;
  store.set(`subdelay:${p.id}:${p.subKey}`, String(p.subDelay));
  updateSubControls();
  applySubDelay();
  showOsd(`Subtitle sync ${p.subDelay > 0 ? '+' : ''}${p.subDelay.toFixed(1)}s`);
}

async function uploadSubtitle(file) {
  const p = state.player;
  if (!p || !file) return;
  setSubNote(`Adding ${file.name}…`);
  try {
    const { key } = await api(`/api/subs/${p.id}?name=${encodeURIComponent(file.name)}`, { method: 'POST', body: file });
    await refreshSubtitles(key);
  } catch (err) {
    setSubNote(err.message, true);
  }
}

$('#subsSelect').addEventListener('change', (e) => selectSubtitle(e.target.value));
$('#subsAdd').addEventListener('click', () => $('#subsFile').click());
$('#subsFile').addEventListener('change', (e) => {
  uploadSubtitle(e.target.files[0]);
  e.target.value = '';
});
$('#subsRemove').addEventListener('click', async () => {
  const p = state.player;
  if (!p?.subKey?.startsWith('u:') || !confirm('Remove this subtitle file?')) return;
  await api(`/api/subs/${p.id}/${encodeURIComponent(p.subKey)}`, { method: 'DELETE' }).catch((err) =>
    setSubNote(err.message, true),
  );
  refreshSubtitles('');
});
$('#subsDelayBox').addEventListener('click', (e) => {
  const btn = e.target.closest('[data-delay]');
  if (btn) changeSubDelay(Number(btn.dataset.delay));
});

const playerDialog = $('#playerDialog');
playerDialog.addEventListener('dragover', (e) => {
  e.preventDefault();
  playerDialog.classList.add('dragging');
});
playerDialog.addEventListener('dragleave', (e) => {
  if (!playerDialog.contains(e.relatedTarget)) playerDialog.classList.remove('dragging');
});
playerDialog.addEventListener('drop', (e) => {
  e.preventDefault();
  playerDialog.classList.remove('dragging');
  uploadSubtitle(e.dataTransfer.files[0]);
});

// Report player events to the server (data/media.log) so playback problems on TVs can be diagnosed.
function reportPlayer(event, extra = {}) {
  const p = state.player;
  const body = {
    event,
    src: video.getAttribute('src'),
    t: Math.round((p?.offset || 0) + (video.currentTime || 0)),
    readyState: video.readyState,
    networkState: video.networkState,
    error: video.error ? { code: video.error.code, message: video.error.message } : null,
    ...extra,
  };
  fetch('/api/client-log', { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify(body) }).catch(
    () => {},
  );
}
['loadedmetadata', 'canplay', 'playing', 'waiting', 'stalled', 'error', 'ended'].forEach((ev) =>
  video.addEventListener(ev, () => state.player && reportPlayer(ev)),
);
reportPlayer.capabilities = () => {
  const types = [
    'video/mp4; codecs="avc1.640028, mp4a.40.2"',
    'video/mp4; codecs="hev1.1.6.L120.90"',
    'video/webm; codecs="vp9, opus"',
    'video/webm',
    'video/x-matroska',
    'audio/mp4; codecs="ac-3"',
    'application/vnd.apple.mpegurl',
  ];
  const can = Object.fromEntries(types.map((t) => [t, video.canPlayType(t) || 'no']));
  return { ua: navigator.userAgent, mse: typeof MediaSource !== 'undefined', can };
};

video.addEventListener('error', () => {
  const p = state.player;
  if (!p || !video.getAttribute('src')) return;
  if (!p.compat && state.config?.ffmpeg) {
    p.autoNote = 'This device could not play the file directly, so it is being converted';
    $('#qualitySelect').value = 'remux';
    applyQuality('remux', currentPosition(p));
  } else {
    $('#playerNote').classList.add('error');
    $('#playerNote').textContent = state.config?.ffmpeg
      ? 'Playback failed. Try a lower Quality setting.'
      : 'Your browser cannot play this file format/codec. Install ffmpeg (and restart the app) to enable conversion.';
  }
});

video.addEventListener('loadedmetadata', () => {
  const p = state.player;
  if (!p) return;
  if (p.pendingSeek != null) {
    video.currentTime = p.pendingSeek;
    p.pendingSeek = null;
  }
  if (!p.compat && video.videoWidth === 0) {
    const hint = state.config?.ffmpeg ? 'Set Quality to “Original (converted)”.' : 'Install ffmpeg to enable conversion.';
    $('#playerNote').textContent = `No picture? The video codec (e.g. HEVC) may not be supported by your browser. ${hint}`;
  }
});

video.addEventListener('timeupdate', () => {
  const p = state.player;
  if (!p) return;
  saveProgress();
  if (!p.compat) return;
  const now = p.offset + video.currentTime;
  if (!seeking) $('#seekRange').value = Math.floor(now);
  $('#seekTime').textContent = `${clock(now)} / ${clock(p.duration)}`;
});
video.addEventListener('pause', () => saveProgress(true));
video.addEventListener('ended', () => {
  const p = state.player;
  if (!p) return;
  state.progress[p.id] = { t: 0, duration: p.duration, index: p.index, updatedAt: Date.now() };
  fetch(`/api/progress/${p.id}`, {
    method: 'PUT',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ t: 0, duration: p.duration, index: p.index }),
  }).catch(() => {});
});

let seeking = false;
$('#seekRange').addEventListener('input', () => {
  seeking = true;
  $('#seekTime').textContent = `${clock($('#seekRange').value)} / ${clock(state.player?.duration)}`;
});
$('#seekRange').addEventListener('change', () => {
  seeking = false;
  applyQuality(state.player.quality, Number($('#seekRange').value));
});

$('#fileSelect').addEventListener('change', (e) => {
  Object.assign(state.player, { index: Number(e.target.value), duration: null });
  applyQuality(state.player.quality, 0);
  refreshSubtitles();
});

$('#playerDialog').addEventListener('close', () => {
  saveProgress(true);
  destroyHls();
  video.pause();
  video.removeAttribute('src');
  clearTracks();
  video.load();
  state.player = null;
  $('#settingsPanel').classList.add('hidden');
  renderLocalRows();
  updateDetailsAction();
  updateHeroAction();
});

// ---------- dialogs ----------
document.querySelectorAll('dialog:not(#setupDialog)').forEach((dlg) => {
  dlg.addEventListener('click', (e) => {
    if (e.target === dlg || e.target.closest('[data-close]')) dlg.close();
  });
});
$('#detailsDialog').addEventListener('close', () => (state.detailsId = null));

// ---------- TV remote / keyboard navigation ----------
const FOCUSABLE =
  'button:not([disabled]):not([tabindex="-1"]), a[href]:not(.hidden), input:not([type="hidden"]):not([hidden]), select, video[tabindex="0"]';
const BACK_KEYS = new Set(['Escape', 'GoBack', 'BrowserBack', 'Backspace']);

function navScope() {
  const dialogs = [...document.querySelectorAll('dialog[open]')];
  if (dialogs.length) return dialogs[dialogs.length - 1];
  if ($('#drawer').classList.contains('open')) return $('#drawer');
  return document.body;
}

function isVisible(el) {
  if (el.closest('[hidden], .hidden')) return false;
  const r = el.getBoundingClientRect();
  return r.width > 0 && r.height > 0;
}

/** Move focus to the nearest focusable element in the given direction (spatial navigation). */
function moveFocus(dir) {
  const scope = navScope();
  const active = document.activeElement;
  const candidates = [...scope.querySelectorAll(FOCUSABLE)].filter((el) => el !== active && isVisible(el));
  if (!active || active === document.body || !scope.contains(active)) {
    const first = scope === document.body && !$('#hero').hidden && !$('#homeView').hidden ? $('#heroPrimary') : candidates[0];
    first?.focus();
    return Boolean(first);
  }
  const a = active.getBoundingClientRect();
  const ax = a.left + a.width / 2;
  const ay = a.top + a.height / 2;
  let best = null;
  let bestScore = Infinity;
  for (const el of candidates) {
    const r = el.getBoundingClientRect();
    const cx = r.left + r.width / 2;
    const cy = r.top + r.height / 2;
    let main;
    let cross;
    if (dir === 'right') {
      if (r.left < a.right - 2) continue;
      main = r.left - a.right;
      cross = Math.abs(cy - ay);
    } else if (dir === 'left') {
      if (r.right > a.left + 2) continue;
      main = a.left - r.right;
      cross = Math.abs(cy - ay);
    } else if (dir === 'down') {
      if (r.top < a.bottom - 2) continue;
      main = r.top - a.bottom;
      cross = Math.abs(cx - ax);
    } else {
      if (r.bottom > a.top + 2) continue;
      main = a.top - r.bottom;
      cross = Math.abs(cx - ax);
    }
    const score = main + cross * (dir === 'left' || dir === 'right' ? 3 : 1.2);
    if (score < bestScore) {
      bestScore = score;
      best = el;
    }
  }
  if (!best) return false;
  best.focus({ preventScroll: true });
  best.scrollIntoView({ block: dir === 'up' || dir === 'down' ? 'center' : 'nearest', inline: 'nearest', behavior: 'smooth' });
  if (scope === document.body && best.closest('#hero')) window.scrollTo({ top: 0, behavior: 'smooth' });
  return true;
}

function goBack() {
  const dialogs = [...document.querySelectorAll('dialog[open]:not(#setupDialog)')];
  if (dialogs.length) return dialogs[dialogs.length - 1].close(), true;
  if ($('#drawer').classList.contains('open')) return toggleDrawer(false), true;
  if (!$('#searchView').hidden) return goHome(), true;
  return false;
}

document.addEventListener('keydown', (e) => {
  const target = e.target;
  const typing = target.matches?.('input[type="search"], input[type="text"], textarea');
  const onSelect = target.tagName === 'SELECT';

  // LG/webOS Back (461), Escape, Backspace outside text fields
  if (e.keyCode === 461 || (BACK_KEYS.has(e.key) && !(typing && e.key === 'Backspace'))) {
    if (typing && e.key === 'Escape') {
      target.blur();
      e.preventDefault();
      return;
    }
    if (e.key === 'Escape' && document.querySelector('dialog[open]')) return; // the dialog closes itself
    if (goBack()) e.preventDefault();
    return;
  }

  // Player shortcuts (when focus is on the video, not on a control)
  if (state.player && !typing && !onSelect) {
    const onVideo = target === video || target === document.body || target === playerDialog;
    if (e.key === 'g' || e.key === 'G') return changeSubDelay(-0.1);
    if (e.key === 'h' || e.key === 'H') return changeSubDelay(0.1);
    if (['MediaPlayPause', 'MediaPlay', 'MediaPause'].includes(e.key) || [415, 19].includes(e.keyCode)) {
      e.preventDefault();
      return togglePlay();
    }
    if (e.key === 'MediaFastForward' || e.keyCode === 417) return seekBy(30);
    if (e.key === 'MediaRewind' || e.keyCode === 412) return seekBy(-30);
    if (onVideo) {
      if (e.key === 'ArrowRight') return e.preventDefault(), seekBy(10);
      if (e.key === 'ArrowLeft') return e.preventDefault(), seekBy(-10);
      if (e.key === 'Enter' || e.key === ' ') return e.preventDefault(), togglePlay();
    }
  }

  const dir = { ArrowLeft: 'left', ArrowRight: 'right', ArrowUp: 'up', ArrowDown: 'down' }[e.key];
  if (!dir || onSelect) return;
  if (typing && (dir === 'left' || dir === 'right')) return;
  if (moveFocus(dir)) e.preventDefault();
});

// ---------- setup & settings ----------
// First run: the setup can't be dismissed. Later (Settings): saved passwords stay unless a new one is typed.
function showSetup(firstRun = true) {
  const form = $('#setupForm');
  const d = state.config.defaults || {};
  form.dataset.firstRun = firstRun ? '1' : '';
  form.username.value = d.username || '';
  form.qbitUrl.value = d.qbitUrl || 'http://127.0.0.1:8080';
  form.qbitUsername.value = d.qbitUsername || 'admin';
  form.opensubtitlesUsername.value = d.opensubtitlesUsername || '';
  for (const [name, saved] of [
    ['password', d.hasPassword],
    ['qbitPassword', d.hasQbitPassword],
    ['opensubtitlesKey', d.opensubtitlesKey],
  ]) {
    form[name].value = '';
    form[name].placeholder = saved && !firstRun ? '•••••• (saved — leave empty to keep)' : '';
  }
  $('#setupTitle').textContent = firstRun ? "Welcome! Let's connect your accounts." : 'Settings';
  $('#setupCancel').hidden = firstRun;
  if (firstRun) $('#hero').hidden = true;
  $('#setupDialog').showModal();
  form.username.focus();
}
$('#setupDialog').addEventListener('cancel', (e) => {
  if ($('#setupForm').dataset.firstRun) e.preventDefault(); // can't be dismissed before it's done
});
$('#setupCancel').addEventListener('click', () => $('#setupDialog').close());
$('#settingsBtn').addEventListener('click', () => state.config && showSetup(false));
$('#setupForm').addEventListener('submit', async (e) => {
  e.preventDefault();
  const form = e.target;
  const btn = form.querySelector('button[type="submit"]');
  form.querySelectorAll('[data-error]').forEach((el) => (el.textContent = ''));
  if (form.dataset.firstRun && !form.password.value && !form.passkey.value) {
    form.querySelector('[data-error="filelist"]').textContent = 'Enter your FileList password or passkey.';
    return;
  }
  btn.disabled = true;
  btn.textContent = 'Checking…';
  try {
    const res = await fetch('/api/setup', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(Object.fromEntries(new FormData(form))),
    });
    const data = await res.json().catch(() => ({}));
    if (!res.ok) {
      form.querySelector(`[data-error="${data.field || 'filelist'}"]`).textContent = data.error || `HTTP ${res.status}`;
      return;
    }
    location.reload();
  } finally {
    btn.disabled = false;
    btn.textContent = 'Save';
  }
});

// ---------- boot ----------
(async () => {
  try {
    reportPlayer('page-load', reportPlayer.capabilities());
  } catch {
    /* diagnostics only */
  }
  try {
    state.config = await api('/api/config');
    $('#categorySelect').insertAdjacentHTML(
      'beforeend',
      state.config.categories.map((c) => `<option value="${c.id}">${esc(c.name)}</option>`).join(''),
    );
  } catch (err) {
    toast(err.message, true);
  }
  updateNav();
  if (state.config?.setup) return showSetup();
  buildRows();
  pollDownloads();
  loadProgress();
})();
