// ==UserScript==
// @name         StreamDock · 发送到下载器
// @namespace    streamdock
// @version      1.2.4
// @description  智能识别详情页名称、M3U8 分辨率/时长/加密、播放器令牌会话与字幕，无媒体时默认自动隐藏悬浮按钮
// @match        http://*/*
// @match        https://*/*
// @run-at       document-start
// @grant        unsafeWindow
// @grant        GM_getValue
// @grant        GM_setValue
// @grant        GM_registerMenuCommand
// @grant        GM_xmlhttpRequest
// @grant        GM_cookie
// @connect      *
// ==/UserScript==

(() => {
  'use strict';

  const allowedHeaders = ['referer', 'origin', 'user-agent', 'authorization', 'cookie'];
  const fileFormats = [
    { ext: 'mp4', mime: 'video/mp4', aliases: ['application/mp4'] },
    { ext: 'webm', mime: 'video/webm', aliases: [] },
    { ext: 'mkv', mime: 'video/x-matroska', aliases: [] },
    { ext: 'mov', mime: 'video/quicktime', aliases: [] },
    { ext: 'mp3', mime: 'audio/mpeg', aliases: ['audio/mp3', 'audio/x-mp3', 'audio/x-mpeg'] },
    { ext: 'm4a', mime: 'audio/mp4', aliases: ['audio/m4a', 'audio/x-m4a'] },
    { ext: 'aac', mime: 'audio/aac', aliases: ['audio/aacp', 'audio/x-aac'] },
    { ext: 'wav', mime: 'audio/wav', aliases: ['audio/wave', 'audio/x-wav', 'audio/vnd.wave'] },
    { ext: 'flac', mime: 'audio/flac', aliases: ['audio/x-flac'] },
    { ext: 'ogg', mime: 'audio/ogg', aliases: ['application/ogg'] },
    { ext: 'opus', mime: 'audio/opus', aliases: [] },
    { ext: 'weba', mime: 'audio/webm', aliases: [] },
    { ext: 'vtt', mime: 'text/vtt', aliases: [] },
    { ext: 'srt', mime: 'application/x-subrip', aliases: ['text/srt', 'text/x-srt'] },
    { ext: 'ass', mime: 'text/x-ass', aliases: ['text/ass'] },
    { ext: 'ssa', mime: 'text/x-ssa', aliases: ['text/ssa'] },
  ];
  function httpUrl(value, base) {
    try {
      const url = new URL(value, base);
      if (!['http:', 'https:'].includes(url.protocol) || url.username || url.password) return '';
      url.hash = '';
      return url.href;
    } catch { return ''; }
  }
  function mediaType(url, mime = '', disposition = '') {
    const parsed = new URL(url);
    const attachment = disposition || parsed.searchParams.get('response-content-disposition') || '';
    const encoded = /filename\*\s*=\s*(?:UTF-8'[^']*')?([^;]+)/i.exec(attachment)?.[1];
    let filename = /filename\s*=\s*(?:"([^"]*)"|([^;]*))/i.exec(attachment)?.slice(1).find(Boolean) || '';
    if (encoded) {
      try { filename = decodeURIComponent(encoded.trim().replace(/^"|"$/g, '')); } catch { filename = ''; }
    }
    const names = [parsed.pathname, filename.trim(), parsed.searchParams.get('filename') || ''];
    const contentType = (mime || parsed.searchParams.get('response-content-type') || '').split(';')[0].trim().toLowerCase();
    if (/mpegurl/i.test(contentType) || names.some(name => /\.m3u8?$/i.test(name))) return 'application/vnd.apple.mpegurl';
    // Explicit filenames distinguish Opus from other Ogg audio without re-encoding.
    const named = fileFormats.find(format => names.some(name => name.toLowerCase().endsWith(`.${format.ext}`)));
    const typed = fileFormats.find(format => format.mime === contentType || format.aliases.includes(contentType));
    return (named || typed)?.mime || '';
  }
  function cleanHeaders(input) {
    const output = {};
    for (const [name, value] of Object.entries(input || {})) {
      const key = name.toLowerCase();
      if (allowedHeaders.includes(key) && typeof value === 'string' && !/[\r\n]/.test(value)) output[key] = value;
    }
    return output;
  }
  function scopedHeaders(headers, from, to) {
    const output = cleanHeaders(headers);
    if (new URL(from).origin !== new URL(to).origin) {
      delete output.cookie;
      delete output.authorization;
    }
    return output;
  }
  function referrerFor(page, target, policy = 'strict-origin-when-cross-origin') {
    const from = new URL(page);
    const to = new URL(target);
    const same = from.origin === to.origin;
    const downgrade = from.protocol === 'https:' && to.protocol === 'http:';
    if (policy === 'no-referrer' || (policy === 'same-origin' && !same)) return '';
    if (policy === 'unsafe-url') return from.href;
    if (downgrade && policy !== 'origin' && policy !== 'origin-when-cross-origin') return '';
    if (policy === 'no-referrer-when-downgrade' || (same && !['origin', 'strict-origin'].includes(policy))) return from.href;
    return `${from.origin}/`;
  }
  function cookieHeader(cookies, target) {
    const url = new URL(target);
    return cookies.filter(cookie => {
      const domain = cookie.domain.replace(/^\./, '').toLowerCase();
      const path = cookie.path || '/';
      return (cookie.hostOnly ? url.hostname === domain : url.hostname === domain || url.hostname.endsWith(`.${domain}`)) &&
        (url.pathname === path || (url.pathname.startsWith(path) && (path.endsWith('/') || url.pathname[path.length] === '/'))) &&
        (!cookie.secure || url.protocol === 'https:') && (!cookie.expirationDate || cookie.expirationDate > Date.now() / 1000);
    }).sort((a, b) => (b.path || '/').length - (a.path || '/').length)
      .map(cookie => `${cookie.name}=${cookie.value}`).join('; ');
  }
  const GENERIC_TITLE_RE = /^(?:loading|player|video\s*player|untitled|document|index|play|watch|home|just\s*a\s*moment|attention\s*required|checking\s*your\s*browser|404|403|502|503|加载中|正在加载|请稍候|缓冲中|播放器|视频播放|在线播放|视频详情|无标题|未知视频|首页|未命名)(?:[\s.!…·-]+)?$/i;
  const TITLE_NOISE_SEGMENT_RE = /^(?:(?:免费)?(?:在线|高清|超清|蓝光|流畅)?(?:观看|播放|完整版|未删减)|播放器|影视官网|[a-z0-9-]+(?:\.[a-z0-9-]+)+)$/i;
  const VIDEO_LD_TYPE_RE = /^(?:videoobject|movie|tvepisode|tvseries|episode|clip)$/i;
  function cleanTitleCandidate(raw, host = '') {
    let text = String(raw || '').replace(/\s+/g, ' ').trim();
    if (!text || text.length < 2) return '';
    if (/^https?:\/\//i.test(text)) return '';
    const lowerHost = host.replace(/^www\./i, '').toLowerCase();
    while (true) {
      const match = text.match(/^(.*?)\s*[-_|—·]\s*([^-_|—·]+)$/);
      if (!match) break;
      const head = match[1].trim();
      const tail = match[2].trim();
      const tailNorm = tail.replace(/^www\./i, '').toLowerCase();
      if (head.length >= 2 && (!tail || TITLE_NOISE_SEGMENT_RE.test(tail) || (lowerHost && tailNorm === lowerHost))) {
        text = head;
      } else {
        break;
      }
    }
    const textNorm = text.replace(/^www\./i, '').toLowerCase();
    if (!text || text.length < 2 || GENERIC_TITLE_RE.test(text) || (lowerHost && textNorm === lowerHost)) return '';
    return text.slice(0, 160);
  }
  function findJsonLdTitle(node, host, depth = 0) {
    if (!node || depth > 4) return '';
    if (Array.isArray(node)) {
      for (const item of node) {
        const hit = findJsonLdTitle(item, host, depth + 1);
        if (hit) return hit;
      }
      return '';
    }
    if (typeof node !== 'object') return '';
    const rawTypes = Array.isArray(node['@type']) ? node['@type'] : [node['@type']];
    const matched = rawTypes.some(t => typeof t === 'string' && VIDEO_LD_TYPE_RE.test(t.replace(/^https?:\/\/schema\.org\//i, '')));
    if (matched) {
      const hit = cleanTitleCandidate(node.name || node.headline, host);
      if (hit) return hit;
    }
    if (node['@graph']) {
      const hit = findJsonLdTitle(node['@graph'], host, depth + 1);
      if (hit) return hit;
    }
    return '';
  }
  function extractPageTitle(doc, href = '') {
    if (!doc) return '';
    let host = '';
    try { host = href ? new URL(href).hostname : ''; } catch { host = ''; }
    if (typeof doc.querySelectorAll === 'function') {
      try {
        for (const script of doc.querySelectorAll('script[type="application/ld+json"]')) {
          const raw = (script?.textContent || '').trim();
          if (!raw) continue;
          try {
            const hit = findJsonLdTitle(JSON.parse(raw), host);
            if (hit) return hit;
          } catch { /* Ignore malformed JSON-LD blocks. */ }
        }
      } catch { /* Ignore unsupported selectors in mocks. */ }
    }
    if (typeof doc.querySelector === 'function') {
      for (const selector of [
        'meta[property="og:title"]',
        'meta[name="twitter:title"]',
        'meta[name="title"]',
        '[itemprop="name"]',
      ]) {
        try {
          const el = doc.querySelector(selector);
          const candidate = el?.getAttribute?.('content') || el?.textContent || '';
          const hit = cleanTitleCandidate(candidate, host);
          if (hit) return hit;
        } catch { /* Ignore invalid selector */ }
      }
      for (const selector of [
        'h1.video-title', 'h1.title', '.video-title', '.vod-title', '.play-title',
        '.player-title', '.media-title', '#video-title', '[data-video-title]',
        'main h1', 'article h1', 'h1',
      ]) {
        try {
          const el = doc.querySelector(selector);
          const candidate = el?.getAttribute?.('data-video-title') || el?.textContent || '';
          const hit = cleanTitleCandidate(candidate, host);
          if (hit) return hit;
        } catch { /* Ignore invalid selector */ }
      }
    }
    return cleanTitleCandidate(doc.title, host);
  }
  function inferQualityFromUrl(rawUrl) {
    try {
      const parsed = new URL(rawUrl);
      const target = `${parsed.pathname}${parsed.search}`;
      const resMatch = target.match(/(?:^|[\/_.\-=?&])(?:(\d{3,4})x(\d{3,4})|(2160|1440|1080|720|540|480|360|240)p?|(4k|2k))(?=$|[\/_.\-&])/i);
      if (!resMatch) return null;
      if (resMatch[1] && resMatch[2]) {
        const w = Number.parseInt(resMatch[1], 10);
        const h = Number.parseInt(resMatch[2], 10);
        if (w >= 240 && h >= 144) return { width: w, height: h, resolution: `${w}x${h}`, label: h >= 2160 ? '4K' : `${h}p`, inferred: true };
      }
      if (resMatch[3]) {
        const h = Number.parseInt(resMatch[3], 10);
        return { height: h, label: h >= 2160 ? '4K' : `${h}p`, inferred: true };
      }
      if (resMatch[4]) {
        const token = resMatch[4].toLowerCase();
        const h = token === '4k' ? 2160 : 1440;
        return { height: h, label: token.toUpperCase(), inferred: true };
      }
      return null;
    } catch { return null; }
  }
  function formatDuration(seconds) {
    if (!Number.isFinite(seconds) || seconds <= 0) return '';
    const total = Math.round(seconds);
    const hrs = Math.floor(total / 3600);
    const mins = Math.floor((total % 3600) / 60);
    const secs = total % 60;
    const pad = n => String(n).padStart(2, '0');
    return hrs > 0 ? `${hrs}:${pad(mins)}:${pad(secs)}` : `${mins}:${pad(secs)}`;
  }
  function formatBitrate(bps) {
    if (!Number.isFinite(bps) || bps <= 0) return '';
    if (bps >= 1_000_000) return `${(bps / 1_000_000).toFixed(1).replace(/\.0$/, '')} Mbps`;
    return `${Math.max(1, Math.round(bps / 1000))} kbps`;
  }
  function parseHlsAttributes(raw) {
    const attrs = {};
    const re = /([A-Z0-9-]+)=("(?:[^"\\]|\\.)*"|[^,]*)/gi;
    for (const match of raw.matchAll(re)) {
      const key = match[1].toUpperCase();
      let val = match[2].trim();
      if (val.startsWith('"') && val.endsWith('"')) val = val.slice(1, -1);
      attrs[key] = val;
    }
    return attrs;
  }
  function parseM3u8Meta(text, baseUrl = '') {
    if (typeof text !== 'string' || !text.trimStart().startsWith('#EXTM3U')) return null;
    const lines = text.split(/\r?\n/);
    const variantMap = new Map();
    let pending = null;
    let duration = 0;
    let segmentCount = 0;
    let encryption = '';
    for (const rawLine of lines) {
      const line = rawLine.trim();
      if (!line) continue;
      if (line.startsWith('#EXT-X-STREAM-INF:')) {
        const attrs = parseHlsAttributes(line.slice(18));
        const res = attrs.RESOLUTION?.match(/^(\d+)x(\d+)$/i);
        const width = res ? Number.parseInt(res[1], 10) : 0;
        const height = res ? Number.parseInt(res[2], 10) : 0;
        const resolution = width && height ? `${width}x${height}` : '';
        const bwRaw = Number.parseInt(attrs.BANDWIDTH || attrs['AVERAGE-BANDWIDTH'] || '0', 10);
        const bandwidth = Number.isFinite(bwRaw) && bwRaw > 0 ? bwRaw : 0;
        const label = height >= 2160 ? '4K' : height ? `${height}p` : attrs.NAME || formatBitrate(bandwidth);
        pending = { resolution, width, height, bandwidth, label };
        continue;
      }
      if (pending) {
        if (!line.startsWith('#')) {
          const resolved = baseUrl ? httpUrl(line, baseUrl) : line;
          if (resolved) {
            const prev = variantMap.get(resolved);
            if (!prev || pending.height > prev.height || pending.bandwidth > prev.bandwidth) {
              variantMap.set(resolved, { url: resolved, ...pending });
            }
          }
          pending = null;
        }
        continue;
      }
      if (line.startsWith('#EXT-X-KEY:')) {
        const attrs = parseHlsAttributes(line.slice(11));
        const method = (attrs.METHOD || '').toUpperCase();
        if (method && method !== 'NONE' && !encryption) encryption = method;
        continue;
      }
      if (line.startsWith('#EXTINF:')) {
        const sec = Number.parseFloat(line.slice(8).split(',')[0]);
        if (Number.isFinite(sec) && sec >= 0) duration += sec;
        segmentCount += 1;
      }
    }
    const variants = [...variantMap.values()].sort((a, b) => (b.height - a.height) || (b.bandwidth - a.bandwidth));
    return {
      isMaster: variants.length > 0,
      variants,
      duration: segmentCount > 0 ? Math.round(duration) : 0,
      segmentCount,
      encryption,
    };
  }
  function parseApiResponse(status, responseText) {
    const text = String(responseText || '').trim();
    if (text) {
      try {
        const body = JSON.parse(text);
        if (body && typeof body === 'object') {
          if (status >= 200 && status < 300 && body.code === 0) {
            return { ok: true, data: body.data || { added: 0, skipped: 0 }, message: body.message || '' };
          }
          return { ok: false, message: body.message || `服务器拒绝请求 (HTTP ${status})` };
        }
      } catch { /* Fall through to status diagnostics for non-JSON responses. */ }
    }
    if (status === 401) return { ok: false, message: '访问密码 (DS_SECRET) 错误或未授权 (HTTP 401)' };
    if (status === 403) return { ok: false, message: '下载器拒绝访问，请检查访问密码或来源限制 (HTTP 403)' };
    if (status === 404) return { ok: false, message: '未找到 /api/cat-catch 接口，请检查下载器地址是否正确 (HTTP 404)' };
    if (status === 502 || status === 503 || status === 504) return { ok: false, message: `下载器服务暂时不可用或网关超时 (HTTP ${status})` };
    if (status >= 200 && status < 300) return { ok: false, message: '下载器返回了非 JSON 响应，请检查服务器地址是否指向下载器端口' };
    return { ok: false, message: `请求失败 (HTTP ${status || '未知'})` };
  }
  function findCreatePlayerBlock(text) {
    const match = /\bcreatePlayer\s*\(\s*\{/i.exec(String(text || ''));
    if (!match) return '';
    const start = match.index + match[0].length - 1;
    const rest = text.slice(start);
    let depth = 0;
    let quote = '';
    let escaped = false;
    for (let i = 0; i < rest.length; i += 1) {
      const ch = rest[i];
      if (quote) {
        if (escaped) escaped = false;
        else if (ch === '\\') escaped = true;
        else if (ch === quote) quote = '';
        continue;
      }
      if (ch === "'" || ch === '"' || ch === '`') {
        quote = ch;
      } else if (ch === '{') {
        depth += 1;
      } else if (ch === '}') {
        depth -= 1;
        if (depth === 0) return rest.slice(0, i + 1);
      }
    }
    return '';
  }
  function extractJsScalarField(block, name) {
    const escaped = name.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
    const re = new RegExp(`(?:^|[{\\s,])(?:["']?${escaped}["']?)\\s*:\\s*(?:"([^"\\r\\n\\\\]*(?:\\\\.[^"\\r\\n\\\\]*)*)"|'([^'\\r\\n\\\\]*(?:\\\\.[^'\\r\\n\\\\]*)*)'|(-?\\d+(?:\\.\\d+)?))`, 'm');
    const m = re.exec(block);
    if (!m) return undefined;
    if (m[1] !== undefined) return m[1].replace(/\\\//g, '/');
    if (m[2] !== undefined) return m[2].replace(/\\\//g, '/');
    if (m[3] !== undefined) {
      const n = Number(m[3]);
      return Number.isFinite(n) ? n : m[3];
    }
    return undefined;
  }
  function normalizeTokenSessionObject(options, pageHref = '') {
    if (!options || typeof options !== 'object' || Array.isArray(options)) return null;
    const playerUrl = httpUrl(pageHref);
    if (!playerUrl) return null;
    const readStr = key => {
      const val = options[key];
      if (typeof val === 'string') return val.replace(/\\\//g, '/').trim();
      if (typeof val === 'number' && Number.isFinite(val)) return String(val);
      return '';
    };
    const movieId = readStr('movieId');
    const sessionId = readStr('sessionId');
    const apiSecret = readStr('apiSecret');
    if (!movieId || !sessionId || !apiSecret) return null;
    const guidId = readStr('guidId');
    const issuedRaw = options.apiSecretIssuedAt;
    const timeDiffRaw = Number(options.timeDiff);
    const session = {
      origin: new URL(playerUrl).origin,
      playerUrl,
      sessionId,
      movieId,
      guidId,
      apiSecret,
      timeDiffMs: Number.isFinite(timeDiffRaw) ? Math.trunc(timeDiffRaw) : 0,
    };
    if (issuedRaw !== undefined && issuedRaw !== null && issuedRaw !== '') {
      session.apiSecretIssuedAt = issuedRaw;
    }
    return session;
  }
  function extractTokenSessionFromText(text, pageHref = '') {
    if (!httpUrl(pageHref)) return null;
    const block = findCreatePlayerBlock(text);
    if (!block) return null;
    let parsedObj = null;
    try {
      const candidate = JSON.parse(block);
      if (candidate && typeof candidate === 'object' && !Array.isArray(candidate)) parsedObj = candidate;
    } catch { parsedObj = null; }
    if (!parsedObj) {
      parsedObj = {};
      for (const key of ['movieId', 'sessionId', 'guidId', 'apiSecret', 'apiSecretIssuedAt', 'timeDiff']) {
        const val = extractJsScalarField(block, key);
        if (val !== undefined) parsedObj[key] = val;
      }
    }
    return normalizeTokenSessionObject(parsedObj, pageHref);
  }
  function normalizeTokenPathValue(raw) {
    let out = String(raw || '').trim();
    for (let i = 0; i < 2; i += 1) {
      if (/^%(?:2f|252f)/i.test(out)) {
        try { out = decodeURIComponent(out); } catch { break; }
      } else {
        break;
      }
    }
    return out;
  }
  function isSignedHlsCandidate(rawUrl) {
    try {
      const parsed = new URL(rawUrl);
      if (parsed.pathname.includes('/bcdn_token=')) return true;
      return Boolean(parsed.searchParams.get('token') && parsed.searchParams.get('expires'));
    } catch { return false; }
  }
  function applyFreshTokenToUrl(rawUrl, token, expires, tokenPath = '', vid = '') {
    const cleanUrl = httpUrl(rawUrl);
    const cleanToken = String(token || '').trim();
    if (!cleanUrl || !cleanToken) return rawUrl;
    const parsed = new URL(cleanUrl);
    const hasQueryAuth = Boolean(parsed.searchParams.get('token') && parsed.searchParams.get('expires'));
    const normPath = normalizeTokenPathValue(tokenPath);
    const matchesTokenPath = Boolean(
      normPath && (
        parsed.pathname.startsWith(normPath) ||
        /^\/\d{8}(\/.*)$/.test(parsed.pathname) && parsed.pathname.slice(9).startsWith(normPath)
      ),
    );
    if (!hasQueryAuth && !matchesTokenPath) return rawUrl;
    parsed.searchParams.set('token', cleanToken);
    if (expires !== undefined && expires !== null && String(expires).trim()) {
      parsed.searchParams.set('expires', String(expires).trim());
    }
    if (normPath) parsed.searchParams.set('token_path', normPath);
    if (vid !== undefined && vid !== null && String(vid).trim()) {
      parsed.searchParams.set('vid', String(vid).trim());
    }
    return parsed.href;
  }
  function payloadFor(item, title, sourceUrl, cookies) {
    const requestHeaders = cleanHeaders(item.headers);
    if (cookies) requestHeaders.cookie = cookies;
    const ext = `.${fileFormats.find(format => format.mime === item.type)?.ext || 'mp4'}`;
    const stem = (title.trim() || '视频').replace(/[\\/:*?"<>|\x00-\x1f]/g, '_').slice(0, 100);
    const suffix = ['.vtt', '.srt', '.ass', '.ssa'].includes(ext) ? `.${new URL(item.url).pathname.split('/').pop().replace(/\.[^.]+$/, '').slice(0, 36) || '字幕'}` : '';
    const payload = { url: item.url, type: item.type, name: `${stem}${suffix}${ext}`, sourceUrl, requestHeaders };
    if (item.tokenSession && String(item.type || '').includes('mpegurl')) {
      payload.tokenSession = item.tokenSession;
    }
    return payload;
  }
  // Also usable by the repository's Node tests without a browser or extension.
  if (typeof module === 'object' && module.exports) {
    module.exports = {
      httpUrl, mediaType, cleanHeaders, scopedHeaders, referrerFor, cookieHeader, payloadFor,
      extractPageTitle, inferQualityFromUrl, formatDuration, formatBitrate, parseM3u8Meta, parseApiResponse,
      normalizeTokenSessionObject, extractTokenSessionFromText, applyFreshTokenToUrl,
    };
    return;
  }

  const page = unsafeWindow;
  const resources = new Map();
  const variantHints = new Map();
  const messageKind = 'm3u8-dl-resource-v1';
  const tokenSessionMessageKind = 'm3u8-dl-token-session-v1';
  const tokenRefreshMessageKind = 'm3u8-dl-token-refresh-v1';
  const maxPlaylistBytes = 65536;
  let capturedTokenSession = null;
  let capturedTokenSessionKey = '';
  let render = () => {};
  function publishTokenSession(session, forcePost = false) {
    if (!session) return null;
    const key = JSON.stringify(session);
    const changed = key !== capturedTokenSessionKey;
    if (changed) {
      capturedTokenSession = session;
      capturedTokenSessionKey = key;
      let backfilled = false;
      for (const [resKey, item] of resources.entries()) {
        if (!item.tokenSession && String(item.type || '').includes('mpegurl') && isSignedHlsCandidate(item.url)) {
          resources.set(resKey, { ...item, tokenSession: session });
          backfilled = true;
        }
      }
      if (backfilled) render();
    }
    if ((changed || forcePost) && window !== window.top) {
      window.top.postMessage({ kind: tokenSessionMessageKind, tokenSession: session }, '*');
    }
    return capturedTokenSession;
  }
  function captureCreatePlayerOptions(options) {
    try {
      const found = normalizeTokenSessionObject(options, typeof location !== 'undefined' ? location.href : '');
      if (found) publishTokenSession(found, true);
    } catch { /* Never interrupt site createPlayer execution. */ }
  }
  function installCreatePlayerHook() {
    if (!page || (typeof page !== 'object' && typeof page !== 'function')) return;
    try {
      const wrapCreatePlayer = fn => {
        if (typeof fn !== 'function' || fn.__streamdockWrapped) return fn;
        const wrapped = function (...args) {
          captureCreatePlayerOptions(args[0]);
          return fn.apply(this, args);
        };
        try {
          Object.defineProperty(wrapped, '__streamdockWrapped', { value: true, configurable: true });
        } catch { /* Ignore metadata tagging failure on frozen/proxy wrappers. */ }
        return wrapped;
      };
      let currentFn = typeof page.createPlayer === 'function' ? wrapCreatePlayer(page.createPlayer) : page.createPlayer;
      const desc = Object.getOwnPropertyDescriptor(page, 'createPlayer');
      if (desc && desc.configurable === false) {
        if (typeof page.createPlayer === 'function') page.createPlayer = currentFn;
        return;
      }
      Object.defineProperty(page, 'createPlayer', {
        configurable: true,
        enumerable: true,
        get() {
          return currentFn;
        },
        set(nextFn) {
          currentFn = typeof nextFn === 'function' ? wrapCreatePlayer(nextFn) : nextFn;
        },
      });
    } catch { /* Ignore non-configurable global createPlayer properties. */ }
  }
  installCreatePlayerHook();
  function mergeMeta(prev, next, url, type) {
    const merged = { ...prev, ...next };
    if (next && !next.inferred && (next.resolution || next.height || next.label)) delete merged.inferred;
    if (!merged.height && !merged.resolution) {
      const hint = variantHints.get(url);
      if (hint) Object.assign(merged, hint);
      else if (type.includes('mpegurl') || type.startsWith('video/')) {
        const inferred = inferQualityFromUrl(url);
        if (inferred) Object.assign(merged, inferred);
      }
    }
    return Object.keys(merged).length ? merged : undefined;
  }
  function scanTokenSession() {
    if (capturedTokenSession) {
      if (window !== window.top) {
        window.top.postMessage({ kind: tokenSessionMessageKind, tokenSession: capturedTokenSession }, '*');
      }
      return capturedTokenSession;
    }
    if (typeof document === 'undefined' || typeof document.querySelectorAll !== 'function') return capturedTokenSession;
    try {
      for (const scriptEl of document.querySelectorAll('script')) {
        const src = scriptEl?.getAttribute?.('src') || scriptEl?.src || '';
        if (src) continue;
        const text = scriptEl?.textContent || '';
        if (!text || !text.includes('createPlayer')) continue;
        const found = extractTokenSessionFromText(text, typeof location !== 'undefined' ? location.href : '');
        if (found) return publishTokenSession(found, true);
      }
    } catch { /* Ignore DOM script inspection errors in mocks. */ }
    return capturedTokenSession;
  }
  function attachTokenSessionIfEligible(item) {
    if (!item || !String(item.type || '').includes('mpegurl')) return item;
    if (item.tokenSession || !capturedTokenSession || !isSignedHlsCandidate(item.url)) return item;
    return { ...item, tokenSession: capturedTokenSession };
  }
  function record(value, mime = '', headers = {}, cookiesAllowed = true, meta = null) {
    const url = httpUrl(value, location.href);
    if (!url) return;
    const hint = variantHints.get(url);
    const type = mediaType(url, mime) || (hint ? 'application/vnd.apple.mpegurl' : '');
    if (!type) return;
    scanTokenSession();
    const previous = resources.get(url);
    const captured = cleanHeaders(headers);
    const hasCaptured = Object.keys(captured).length > 0;
    const hasNewMeta = Boolean(meta && Object.keys(meta).length);
    const fromMaster = Boolean(meta?.fromMaster);
    const tokenSession = previous?.tokenSession || ((type.includes('mpegurl') && capturedTokenSession && isSignedHlsCandidate(url)) ? capturedTokenSession : undefined);
    // DOM/performance sightings must not replace the more accurate request context unless upgrading metadata.
    if (previous && !hasCaptured && !hasNewMeta && Boolean(previous.tokenSession) === Boolean(tokenSession)) return;
    if (!previous && resources.size >= 200) return;
    const mergedMeta = mergeMeta(previous?.meta, meta, url, type);
    const mergedCookiesAllowed = !previous
      ? cookiesAllowed
      : (fromMaster || !hasCaptured)
        ? previous.cookiesAllowed
        : cookiesAllowed;
    const mergedHeaders = (previous && fromMaster)
      ? previous.headers
      : {
          referer: referrerFor(location.href, url, document.referrerPolicy || undefined),
          'user-agent': navigator.userAgent,
          ...previous?.headers,
          ...captured,
        };
    const item = {
      url, type, frameUrl: location.href,
      cookiesAllowed: mergedCookiesAllowed,
      headers: mergedHeaders,
      ...(mergedMeta ? { meta: mergedMeta } : {}),
      ...(tokenSession ? { tokenSession } : {}),
    };
    resources.set(url, item);
    if (window !== window.top) window.top.postMessage({ kind: messageKind, item }, '*');
    render();
  }
  function ingestM3u8Text(playlistUrl, text, headers = {}, cookiesAllowed = true) {
    const parsed = parseM3u8Meta(text, playlistUrl);
    if (!parsed) return 0;
    if (parsed.isMaster) {
      const variantsSummary = parsed.variants.slice(0, 4).map(v => v.label || v.resolution).filter(Boolean).join(' / ');
      record(playlistUrl, 'application/vnd.apple.mpegurl', headers, cookiesAllowed, {
        isMaster: true,
        variantsCount: parsed.variants.length,
        variantsSummary,
      });
      let added = 0;
      let masterOrigin = '';
      try { masterOrigin = new URL(playlistUrl).origin; } catch { masterOrigin = ''; }
      for (const variant of parsed.variants) {
        const varHeaders = scopedHeaders(headers, playlistUrl, variant.url);
        let sameOrigin = false;
        try { sameOrigin = Boolean(masterOrigin && new URL(variant.url).origin === masterOrigin); } catch { sameOrigin = false; }
        variantHints.set(variant.url, {
          resolution: variant.resolution,
          width: variant.width,
          height: variant.height,
          bandwidth: variant.bandwidth,
          label: variant.label,
          fromMaster: true,
        });
        if (!resources.has(variant.url)) added += 1;
        record(variant.url, 'application/vnd.apple.mpegurl', varHeaders, cookiesAllowed && sameOrigin, {
          resolution: variant.resolution,
          width: variant.width,
          height: variant.height,
          bandwidth: variant.bandwidth,
          label: variant.label,
          fromMaster: true,
        });
      }
      return added;
    }
    record(playlistUrl, 'application/vnd.apple.mpegurl', headers, cookiesAllowed, {
      duration: parsed.duration,
      segmentCount: parsed.segmentCount,
      encryption: parsed.encryption,
    });
    return 0;
  }
  async function readBoundedText(response, limit = maxPlaylistBytes) {
    if (typeof response?.clone !== 'function') return '';
    const cloned = response.clone();
    if (cloned.body && typeof cloned.body.getReader === 'function') {
      const reader = cloned.body.getReader();
      let text = '';
      const decoder = new TextDecoder();
      try {
        while (text.length < limit) {
          const { done, value } = await reader.read();
          if (done) break;
          text += decoder.decode(value.subarray(0, limit - text.length), { stream: true });
          if (text.length <= 16 && text.trimStart().length >= 7 && !text.trimStart().startsWith('#EXTM3U')) return '';
        }
        return text;
      } finally { void reader.cancel().catch(() => {}); }
    }
    if (typeof cloned.text === 'function') return String(await cloned.text()).slice(0, limit);
    return '';
  }
  let updateSelectionKey = () => {};
  function refreshResourcesWithToken(info) {
    const token = String(info?.token || '').trim();
    const expires = info?.expires;
    const tokenPath = info?.tokenPath || info?.token_path || '';
    const vid = info?.vid || '';
    if (!token || expires === undefined || expires === null || !String(expires).trim()) return false;
    let changed = false;
    for (const [oldUrl, item] of [...resources.entries()]) {
      if (!String(item.type || '').includes('mpegurl')) continue;
      const nextUrl = applyFreshTokenToUrl(oldUrl, token, expires, tokenPath, vid);
      if (nextUrl === oldUrl) continue;
      changed = true;
      resources.delete(oldUrl);
      const nextItem = attachTokenSessionIfEligible({ ...item, url: nextUrl });
      resources.set(nextUrl, nextItem);
      if (variantHints.has(oldUrl)) {
        const hint = variantHints.get(oldUrl);
        variantHints.delete(oldUrl);
        variantHints.set(nextUrl, hint);
      }
      updateSelectionKey(oldUrl, nextUrl);
    }
    if (changed) render();
    return changed;
  }
  function handleTokenRefreshResponse(rawUrl, status, bodyText) {
    if (status < 200 || status >= 300 || !bodyText) return;
    let path = '';
    try { path = new URL(rawUrl, location.href).pathname; } catch { path = ''; }
    if (!path.endsWith('/api/token/generate')) return;
    try {
      const parsed = JSON.parse(bodyText);
      const data = parsed?.data && typeof parsed.data === 'object' ? parsed.data : parsed;
      const token = String(data?.token || '').trim();
      const expires = data?.expires;
      const tokenPath = String(data?.tokenPath || data?.token_path || '').trim();
      const vid = data?.vid !== undefined && data?.vid !== null ? String(data.vid).trim() : '';
      if (!token || expires === undefined || expires === null || !String(expires).trim()) return;
      const payload = { token, expires, tokenPath, vid };
      refreshResourcesWithToken(payload);
      if (window !== window.top) {
        window.top.postMessage({ kind: tokenRefreshMessageKind, ...payload }, '*');
      }
    } catch { /* Ignore non-JSON token responses. */ }
  }
  const readHeaders = value => {
    try { return Object.fromEntries(new Headers(value).entries()); } catch { return {}; }
  };
  const originalFetch = page.fetch;
  if (originalFetch) page.fetch = function (input, init) {
    let url = '';
    let headers = {};
    let credentials = 'same-origin';
    try {
      url = httpUrl(typeof input === 'string' || input instanceof URL ? String(input) : input.url, location.href);
      headers = readHeaders(init?.headers || input?.headers);
      credentials = init?.credentials || input?.credentials || credentials;
      if (url) {
        headers.referer ??= referrerFor(location.href, url, init?.referrerPolicy || input?.referrerPolicy || document.referrerPolicy || undefined);
        if ((init?.mode || input?.mode || 'cors') === 'cors' && new URL(url).origin !== location.origin) headers.origin ??= location.origin;
        record(url, '', headers, credentials === 'include' || (credentials === 'same-origin' && new URL(url).origin === location.origin));
      }
    } catch { /* Observing must never prevent the page request. */ }
    return originalFetch.apply(this, arguments).then(response => {
      try {
        const target = httpUrl(response.url || url, location.href);
        if (!target) return response;
        const type = response.headers.get('content-type') || '';
        const disp = response.headers.get('content-disposition') || '';
        const detected = mediaType(target, type, disp) || type;
        const scoped = scopedHeaders(headers, url || target, target);
        const allowed = credentials === 'include' || (credentials === 'same-origin' && new URL(target).origin === location.origin);
        record(target, detected, scoped, allowed);
        const isHlsCandidate = detected === 'application/vnd.apple.mpegurl';
        const isDisguisedText = !mediaType(target, type) && /text\/plain|octet-stream/i.test(type);
        if (isHlsCandidate || isDisguisedText) {
          void (async () => {
            const text = await readBoundedText(response, maxPlaylistBytes);
            if (text.trimStart().startsWith('#EXTM3U')) ingestM3u8Text(target, text, scoped, allowed);
          })().catch(() => {});
        } else if (response.ok && new URL(target).pathname.endsWith('/api/token/generate') && typeof response.clone === 'function') {
          void response.clone().text().then(text => handleTokenRefreshResponse(target, response.status, text)).catch(() => {});
        }
      } catch { /* The original response remains untouched. */ }
      return response;
    });
  };
  const xhrState = new WeakMap();
  const proto = page.XMLHttpRequest.prototype;
  const originalOpen = proto.open;
  const originalSetHeader = proto.setRequestHeader;
  const originalSend = proto.send;
  proto.open = function (method, url) {
    xhrState.set(this, { url: httpUrl(url, location.href), headers: {} });
    return originalOpen.apply(this, arguments);
  };
  proto.setRequestHeader = function (name, value) {
    const result = originalSetHeader.apply(this, arguments);
    const state = xhrState.get(this);
    if (state) state.headers[name.toLowerCase()] = String(value);
    return result;
  };
  proto.send = function () {
    const state = xhrState.get(this);
    if (state?.url) {
      const allowed = this.withCredentials || new URL(state.url).origin === location.origin;
      state.headers.referer ??= referrerFor(location.href, state.url, document.referrerPolicy || undefined);
      if (new URL(state.url).origin !== location.origin) state.headers.origin ??= location.origin;
      record(state.url, '', state.headers, allowed);
      this.addEventListener('load', () => {
        try {
          const target = httpUrl(this.responseURL || state.url, location.href);
          const headers = scopedHeaders(state.headers, state.url, target);
          const mime = this.getResponseHeader('content-type') || '';
          const disp = this.getResponseHeader('content-disposition') || '';
          const detected = mediaType(target, mime, disp) || mime;
          const xhrAllowed = this.withCredentials || new URL(target).origin === location.origin;
          const bodyText = ['', 'text'].includes(this.responseType) ? String(this.responseText || '').slice(0, maxPlaylistBytes) : '';
          if (bodyText.trimStart().startsWith('#EXTM3U')) {
            ingestM3u8Text(target, bodyText, headers, xhrAllowed);
          } else if (this.status >= 200 && this.status < 300 && new URL(target).pathname.endsWith('/api/token/generate') && bodyText) {
            handleTokenRefreshResponse(target, this.status, bodyText);
          } else {
            record(target, detected, headers, xhrAllowed);
          }
        } catch { /* Binary responses do not expose responseText. */ }
      }, { once: true });
    }
    return originalSend.apply(this, arguments);
  };

  let onScan = () => {};
  function scan() {
    scanTokenSession();
    if (typeof performance !== 'undefined') {
      for (const entry of performance.getEntriesByType('resource')) record(entry.name);
    }
    for (const element of document.querySelectorAll('video, audio, source, track')) {
      const crossOrigin = element.crossOrigin || element.parentElement?.crossOrigin;
      record(element.currentSrc || element.src, element.type || (element.tagName === 'TRACK' ? 'text/vtt' : ''), {}, crossOrigin !== 'anonymous');
    }
    onScan();
  }
  try {
    const observer = new PerformanceObserver(list => {
      for (const entry of list.getEntries()) record(entry.name);
    });
    observer.observe({ type: 'resource', buffered: true });
  } catch { /* Older browsers still use DOM/fetch/XHR observation. */ }
  document.addEventListener('loadedmetadata', scan, true);
  document.addEventListener('load', scan, true);
  setInterval(scan, 3000);
  if (window !== window.top) return;

  window.addEventListener('message', event => {
    const kind = event.data?.kind;
    if (kind === tokenSessionMessageKind) {
      const sess = event.data?.tokenSession;
      if (sess && typeof sess === 'object' && sess.origin === event.origin && sess.movieId && sess.sessionId && sess.apiSecret) {
        capturedTokenSession = sess;
        capturedTokenSessionKey = JSON.stringify(sess);
        let updated = false;
        for (const [key, item] of resources.entries()) {
          if (!item.tokenSession && String(item.type || '').includes('mpegurl') && isSignedHlsCandidate(item.url)) {
            resources.set(key, { ...item, tokenSession: sess });
            updated = true;
          }
        }
        if (updated) render();
      }
      return;
    }
    if (kind === tokenRefreshMessageKind) {
      refreshResourcesWithToken(event.data);
      return;
    }
    const item = event.data?.item;
    if (kind !== messageKind || !item || !event.source) return;
    const url = httpUrl(item.url);
    const frameUrl = httpUrl(item.frameUrl);
    const type = mediaType(url, item.type);
    if (!url || !frameUrl || new URL(frameUrl).origin !== event.origin || !type) return;
    const previous = resources.get(url);
    if (resources.size >= 200 && !previous) return;
    const mergedMeta = mergeMeta(previous?.meta, item.meta && typeof item.meta === 'object' ? item.meta : null, url, type);
    const tokenSession = item.tokenSession || previous?.tokenSession || ((type.includes('mpegurl') && capturedTokenSession && isSignedHlsCandidate(url)) ? capturedTokenSession : undefined);
    resources.set(url, {
      url, frameUrl, type,
      headers: { ...previous?.headers, ...cleanHeaders(item.headers) },
      cookiesAllowed: item.cookiesAllowed !== false,
      ...(mergedMeta ? { meta: mergedMeta } : {}),
      ...(tokenSession ? { tokenSession } : {}),
    });
    render();
  });

  function mount() {
    const host = document.createElement('div');
    host.style.cssText = 'position:fixed;inset:0;pointer-events:none;z-index:2147483647;color-scheme:light;';
    const root = host.attachShadow({ mode: 'closed' });
    root.innerHTML = `<style>
      :host{all:initial;font:14px/1.5 system-ui,-apple-system,sans-serif;color:#24334b;pointer-events:none}*{box-sizing:border-box}
      button,input{font:inherit;-webkit-tap-highlight-color:transparent}
      button{cursor:pointer;background:#edf4ff;color:#1760d5;border:1px solid #d6e4fa;border-radius:10px;padding:8px 12px;min-height:38px;font-weight:500;touch-action:manipulation}
      button:disabled{opacity:.5;cursor:wait}
      input[type=text],input[type=password],input[type=url]{width:100%;min-height:42px;border:1px solid #d6dfed;border-radius:9px;padding:8px 10px;color:#24334b;background:#fff;font-size:15px}
      input[type=checkbox]{width:19px;height:19px;flex-shrink:0;accent-color:#1760d5;margin:2px 0 0}
      label{display:block;margin-top:10px;font-size:13px;color:#475569;font-weight:500}
      label input{margin-top:4px;font-weight:400}
      label.check{display:flex;align-items:flex-start;gap:8px;margin-top:12px;color:#24334b;font-weight:400;cursor:pointer}
      label.check input{margin-top:2px}
      h2{font-size:16px;font-weight:600;margin:0}
      #backdrop{display:none}
      #toast{pointer-events:auto;position:fixed;top:max(14px,env(safe-area-inset-top));left:50%;transform:translateX(-50%);z-index:2;max-width:min(400px,calc(100vw - 28px));padding:10px 14px;border-radius:12px;font-size:13px;font-weight:600;line-height:1.45;box-shadow:0 10px 30px rgba(15,23,42,.18);background:#fff;color:#1e293b;border:1px solid #dce5f2;text-align:center}
      #toast[hidden]{display:none}
      #toast.toast-ok{background:#ecfdf5;color:#065f46;border-color:#a7f3d0}
      #toast.toast-err{background:#fef2f2;color:#991b1b;border-color:#fecaca}
      #toast.toast-info{background:#eff6ff;color:#1e40af;border-color:#bfdbfe}
      section{pointer-events:auto;position:fixed;right:16px;bottom:76px;width:min(448px,calc(100vw - 32px));max-height:min(720px,calc(100dvh - 104px));display:flex;flex-direction:column;overflow:hidden;background:#fff;border:1px solid #dce5f2;border-radius:16px;box-shadow:0 12px 44px rgba(15,23,42,.22)}
      section[hidden]{display:none}
      header{display:flex;align-items:center;justify-content:space-between;gap:8px;padding:14px 16px;border-bottom:1px solid #edf1f7;flex-shrink:0}
      .head-title{display:flex;align-items:center;gap:8px;min-width:0}
      .ver{font-size:11px;font-weight:600;color:#64748b;background:#f1f5f9;border:1px solid #e2e8f0;border-radius:999px;padding:1px 7px;flex-shrink:0}
      .body{flex:1;min-height:0;overflow-y:auto;overscroll-behavior:contain;-webkit-overflow-scrolling:touch;padding:12px 16px 16px}
      footer{display:flex;flex-direction:column;gap:8px;padding:12px 16px max(12px,env(safe-area-inset-bottom));border-top:1px solid #edf1f7;background:#f8fafc;flex-shrink:0}
      small{display:block;color:#64748b;margin:4px 0;font-size:12px;line-height:1.5;overflow-wrap:anywhere}
      details{border:1px solid #e6ecf5;border-radius:12px;padding:0 12px;background:#f8fafc;margin-bottom:10px}
      summary{display:flex;align-items:center;justify-content:space-between;cursor:pointer;padding:10px 0;font-size:13px;font-weight:600;color:#334155;list-style:none}
      summary::-webkit-details-marker{display:none}
      summary::after{content:'展开';font-size:12px;font-weight:500;color:#64748b}
      details[open]>summary::after{content:'收起'}
      details .box{padding-bottom:12px;border-top:1px solid #e6ecf5}
      details .box button{margin-top:10px;width:100%}
      .manual-box{margin-top:10px;background:#fff}
      .title-field{margin-top:4px}
      .title-head{display:flex;align-items:center;justify-content:space-between;gap:8px;margin-bottom:4px}
      .title-head span{font-size:13px;color:#475569;font-weight:500}
      .title-actions{display:flex;gap:6px}
      .title-actions button{min-height:30px;padding:3px 9px;font-size:12px;border-radius:8px;background:#f1f5f9;color:#334155;border-color:#cbd5e1}
      .list-bar{display:flex;align-items:center;justify-content:space-between;gap:8px;margin:14px 0 6px;flex-wrap:wrap}
      #count-label{font-size:13px;font-weight:600;color:#334155}
      .list-tools{display:flex;gap:6px;flex-wrap:wrap}
      .list-tools button,.filter-bar button{min-height:34px;padding:5px 10px;font-size:12px}
      .filter-bar{display:flex;gap:6px;margin:0 0 10px;flex-wrap:wrap}
      .chip{background:#f1f5f9;color:#475569;border-color:#e2e8f0;border-radius:999px}
      .chip.active{background:#1760d5;color:#fff;border-color:#1760d5}
      #list{display:flex;flex-direction:column;gap:8px;margin-bottom:12px;color:#64748b;font-size:13px;line-height:1.6}
      .resource{display:flex;align-items:flex-start;gap:10px;padding:11px 12px;margin:0;border:1px solid #e3eaf5;border-radius:11px;background:#fff;font-weight:400;color:#24334b}
      .resource:has(input:checked){background:#f0f6ff;border-color:#1760d5}
      .resource>span{min-width:0;flex:1;font-weight:600;font-size:13px;overflow-wrap:anywhere;cursor:pointer}
      .resource small{font-size:12px;font-weight:400;margin:3px 0 0;color:#64748b}
      .badges{display:flex;flex-wrap:wrap;gap:5px;margin:5px 0 3px}
      .badge{display:inline-flex;align-items:center;flex:0 0 auto;max-width:100%;white-space:nowrap;overflow:hidden;text-overflow:ellipsis;padding:2px 8px;border-radius:999px;font-size:11px;line-height:1.35;font-weight:600;background:#f1f5f9;color:#334155;border:1px solid #e2e8f0}
      .badge-hd{background:#eff6ff;color:#1d4ed8;border-color:#bfdbfe}
      .badge-warn{background:#fffbeb;color:#b45309;border-color:#fde68a}
      .badge-info{background:#eff6ff;color:#1e40af;border-color:#bfdbfe}
      .badge-ok{background:#ecfdf5;color:#047857;border-color:#a7f3d0}
      .badge-err{background:#fef2f2;color:#b91c1c;border-color:#fecaca}
      .row-send{align-self:center;flex-shrink:0;min-height:38px;min-width:54px;padding:6px 11px;font-size:12px;font-weight:600;background:#eff6ff;color:#1760d5;border:1px solid #bfdbfe;border-radius:9px}
      .row-send.sent{background:#ecfdf5;color:#047857;border-color:#a7f3d0}
      .row-send.err{background:#fef2f2;color:#b91c1c;border-color:#fecaca}
      .primary{background:#1760d5;color:#fff;border-color:#1760d5;width:100%;min-height:46px;font-size:15px;font-weight:600}
      .status-wrap{display:flex;align-items:center;justify-content:space-between;gap:8px;flex-wrap:wrap}
      .status-wrap:has(#status:empty){display:none}
      #status{flex:1;white-space:pre-wrap;overflow-wrap:anywhere;margin:0;padding:8px 10px;border-radius:9px;font-size:12px;line-height:1.45;font-weight:500;background:#eff6ff;color:#1e40af;border:1px solid #bfdbfe}
      #status:empty{display:none}
      #status.status-ok{background:#ecfdf5;color:#065f46;border-color:#a7f3d0}
      #status.status-err{background:#fef2f2;color:#991b1b;border-color:#fecaca}
      #open-dl{flex-shrink:0;font-size:12px;font-weight:600;color:#1760d5;text-decoration:none;padding:6px 10px;border-radius:8px;background:#fff;border:1px solid #bfdbfe}
      #open-dl[hidden]{display:none}
      #toggle{pointer-events:auto;position:fixed;right:12px;bottom:max(76px,calc(16px + env(safe-area-inset-bottom)));border-radius:999px;padding:10px 14px;background:#fff;color:#1760d5;border:1px solid #d6e4fa;box-shadow:0 4px 16px rgba(15,23,42,.14);font-weight:600;touch-action:none;user-select:none}
      #toggle.has-media{background:#1760d5;color:#fff;border-color:#1760d5;box-shadow:0 6px 20px rgba(23,96,213,.32)}
      #toggle[hidden]{display:none}
      @media (max-width:640px){
        input[type=text],input[type=password],input[type=url]{font-size:16px}
        #backdrop:not([hidden]){display:block;position:fixed;inset:0;background:rgba(15,23,42,.38);pointer-events:auto;touch-action:none;overscroll-behavior:contain}
        section{left:0;right:0;bottom:0;width:100%;max-width:100%;max-height:86dvh;border-radius:20px 20px 0 0;border-bottom:0;border-inline:0}
        header{position:relative;padding:20px 16px 12px}
        header::before{content:'';position:absolute;top:8px;left:calc(50% - 18px);width:36px;height:4px;border-radius:4px;background:#cbd5e1}
        .row-send{min-height:42px;min-width:58px}
      }
    </style><div id="toast" role="status" aria-live="polite" hidden></div><div id="backdrop" hidden></div><section hidden aria-label="发送媒体到 StreamDock"><header><div class="head-title"><h2>发送到 StreamDock</h2><span class="ver">v1.2.4</span></div><button id="close" aria-label="关闭">关闭</button></header>
      <div class="body">
        <details id="settings"><summary>连接设置</summary><div class="box"><label>StreamDock 地址<input id="server" type="url" placeholder="http://192.168.1.10:6600" /></label>
        <label>访问密码（DS_SECRET）<input id="secret" type="password" autocomplete="off" /></label>
        <label class="check"><input id="hide-empty" type="checkbox" checked /><span>无媒体时自动隐藏悬浮按钮（可从脚本猫菜单打开面板）</span></label><button id="save">保存连接设置</button></div></details>
        <div class="title-field"><div class="title-head"><span>下载名称</span><div class="title-actions"><button type="button" id="resync-title">重抓</button><button type="button" id="clear-title">清空</button></div></div><input id="title" type="text" placeholder="自动识别或手动输入名称" /></div>
        <div class="list-bar"><span id="count-label">检测到 0 个媒体</span><div class="list-tools"><button type="button" id="probe">探测清晰度</button><button type="button" id="all">全选 / 取消</button><button type="button" id="refresh">重新扫描</button></div></div>
        <div class="filter-bar"><button type="button" id="filter-all" class="chip active">全部 (0)</button><button type="button" id="filter-video" class="chip">视频 (0)</button><button type="button" id="filter-sub" class="chip">字幕 (0)</button></div>
        <div id="list"></div>
        <details id="advanced"><summary>更多选项（详情页 · Cookie · 手动添加）</summary><div class="box"><label>详情页地址<input id="source" type="url" /></label>
        <label class="check"><input id="cookies" type="checkbox" checked /><span>附带媒体域 Cookie（首次需脚本猫授权）</span></label>
        <small>名称和详情页可修改；Cookie 只在点击发送时读取。Referer / Origin 根据播放页请求推导，不等于浏览器网络抓包。</small>
        <details class="manual-box"><summary>手动添加媒体地址</summary><div class="box"><label>M3U8 / 视频 / 音频 / 字幕直链<input id="manual-url" type="url" placeholder="https://…" /></label><button id="manual-add">添加到列表</button><small>无后缀且无法识别的地址按 M3U8 添加。请求来源默认使用当前播放页。</small></div></details></div></details>
      </div>
      <footer><div class="status-wrap"><small id="status" role="status"></small><a id="open-dl" target="_blank" rel="noopener noreferrer" hidden>打开下载器 ↗</a></div><button id="send" class="primary">发送所选</button></footer>
      </section><button id="toggle" hidden>下载 · 0</button>`;
    document.documentElement.append(host);
    const $ = id => root.getElementById(id);
    const panel = root.querySelector('section');
    const selected = new Set();
    const sendStates = new Map();
    updateSelectionKey = (oldUrl, nextUrl) => {
      if (selected.has(oldUrl)) {
        selected.delete(oldUrl);
        selected.add(nextUrl);
      }
      if (sendStates.has(oldUrl)) {
        const st = sendStates.get(oldUrl);
        sendStates.delete(oldUrl);
        sendStates.set(nextUrl, st);
      }
    };
    let busy = false;
    let probing = false;
    let dragged = false;
    let activeFilter = 'all';
    let titleEdited = false;
    let sourceEdited = false;
    let lastHref = typeof location !== 'undefined' ? location.href : '';
    let toastTimer = null;

    const showToast = (text, tone = 'info') => {
      const toast = $('toast');
      if (!toast) return;
      if (toastTimer && typeof clearTimeout === 'function') clearTimeout(toastTimer);
      toast.textContent = text;
      toast.className = `toast-${tone}`;
      toast.hidden = false;
      if (typeof setTimeout === 'function') {
        toastTimer = setTimeout(() => { toast.hidden = true; }, 4500);
        toastTimer?.unref?.();
      }
    };
    const setStatus = (text, tone = 'info', toast = false) => {
      const el = $('status');
      if (el) {
        el.textContent = text;
        el.className = text ? `status-${tone}` : '';
      }
      if (toast && text) showToast(text, tone);
    };
    const status = text => setStatus(text, 'info', false);
    const isSubtitleItem = item => item.type.startsWith('text/') || item.type.includes('subrip');
    const getVisibleItems = () => {
      const list = [...resources.values()];
      if (activeFilter === 'video') return list.filter(item => !isSubtitleItem(item));
      if (activeFilter === 'sub') return list.filter(item => isSubtitleItem(item));
      return list;
    };
    const syncPageContext = (forceTitle = false) => {
      const href = typeof location !== 'undefined' ? location.href : '';
      if (href && href !== lastHref) {
        lastHref = href;
        titleEdited = false;
        sourceEdited = false;
      }
      if (!sourceEdited && href && $('source')) $('source').value = href;
      if ((forceTitle || !titleEdited) && $('title')) {
        const extracted = extractPageTitle(document, href);
        if (extracted) $('title').value = extracted;
        else if (forceTitle) $('title').value = document.title || '';
      }
    };
    $('title').oninput = () => { titleEdited = true; };
    if ($('source')) $('source').oninput = () => { sourceEdited = true; };
    onScan = () => { if (!panel.hidden) syncPageContext(false); };

    const settings = GM_getValue('docker-settings', {}) || {};
    $('server').value = settings.server || '';
    $('secret').value = settings.secret || '';
    if ($('hide-empty')) $('hide-empty').checked = settings.hideWhenEmpty !== false;
    $('settings').open = !settings.server;
    const toggleBtn = $('toggle');
    const isSameOriginServer = () => {
      try {
        const srv = httpUrl(($('server')?.value || settings.server || '').trim());
        return Boolean(srv && typeof location !== 'undefined' && new URL(srv).origin === location.origin);
      } catch { return false; }
    };
    const isHideWhenEmpty = () => ($('hide-empty') ? Boolean($('hide-empty').checked) : settings.hideWhenEmpty !== false);
    const syncToggleVisibility = () => {
      if ($('backdrop')) $('backdrop').hidden = panel.hidden;
      toggleBtn.hidden = !panel.hidden || isSameOriginServer() || (isHideWhenEmpty() && resources.size === 0);
    };
    syncToggleVisibility();
    const clampBottom = value => {
      const max = typeof window.innerHeight === 'number' && window.innerHeight > 120 ? window.innerHeight - 64 : 600;
      return Math.max(16, Math.min(max, Math.round(value)));
    };
    const savedBottom = GM_getValue('docker-btn-bottom', null);
    if (typeof savedBottom === 'number' && Number.isFinite(savedBottom) && toggleBtn.style) {
      toggleBtn.style.bottom = `${clampBottom(savedBottom)}px`;
    }
    if (typeof toggleBtn.addEventListener === 'function') {
      let startY = 0;
      let startBottom = 76;
      let pointerId = null;
      const currentBottom = () => {
        const inline = Number.parseFloat(toggleBtn.style?.bottom);
        if (Number.isFinite(inline)) return inline;
        if (typeof toggleBtn.getBoundingClientRect === 'function' && typeof window.innerHeight === 'number') {
          return window.innerHeight - toggleBtn.getBoundingClientRect().bottom;
        }
        return 76;
      };
      toggleBtn.addEventListener('pointerdown', e => {
        if (e.button && e.button !== 0) return;
        pointerId = e.pointerId;
        startY = e.clientY;
        startBottom = currentBottom();
        dragged = false;
        try { toggleBtn.setPointerCapture?.(e.pointerId); } catch { /* Ignore unsupported pointer capture. */ }
      });
      toggleBtn.addEventListener('pointermove', e => {
        if (pointerId !== e.pointerId) return;
        const dy = startY - e.clientY;
        if (!dragged && Math.abs(dy) < 8) return;
        dragged = true;
        if (toggleBtn.style) toggleBtn.style.bottom = `${clampBottom(startBottom + dy)}px`;
      });
      const finishDrag = e => {
        if (pointerId !== e.pointerId) return;
        pointerId = null;
        if (dragged) GM_setValue('docker-btn-bottom', currentBottom());
      };
      toggleBtn.addEventListener('pointerup', finishDrag);
      toggleBtn.addEventListener('pointercancel', finishDrag);
    }
    const closePanel = () => {
      panel.hidden = true;
      syncToggleVisibility();
    };
    function open() {
      panel.hidden = !panel.hidden;
      syncToggleVisibility();
      if (!panel.hidden) syncPageContext(false);
      scan();
      render();
    }
    toggleBtn.onclick = () => {
      if (dragged) { dragged = false; return; }
      open();
    };
    $('close').onclick = closePanel;
    if ($('backdrop')) $('backdrop').onclick = closePanel;
    GM_registerMenuCommand('打开媒体发送面板', () => { if (panel.hidden) open(); });
    if ($('hide-empty')) {
      $('hide-empty').onchange = () => {
        settings.hideWhenEmpty = isHideWhenEmpty();
        const saved = GM_getValue('docker-settings', {}) || {};
        GM_setValue('docker-settings', { ...saved, hideWhenEmpty: settings.hideWhenEmpty });
        syncToggleVisibility();
      };
    }
    $('save').onclick = () => {
      const server = httpUrl($('server').value.trim());
      if (!server || !$('secret').value.trim()) { setStatus('请填写有效的下载器地址及 DS_SECRET。', 'err', true); return; }
      const cleanServer = server.replace(/\/$/, '');
      const hideWhenEmpty = isHideWhenEmpty();
      settings.server = cleanServer;
      settings.hideWhenEmpty = hideWhenEmpty;
      GM_setValue('docker-settings', { server: cleanServer, secret: $('secret').value.trim(), hideWhenEmpty });
      syncToggleVisibility();
      setStatus('连接设置已保存在脚本猫中。', 'ok', true);
      $('settings').open = false;
    };
    if ($('resync-title')) {
      $('resync-title').onclick = () => {
        titleEdited = false;
        syncPageContext(true);
        const val = $('title').value.trim();
        setStatus(val ? `已识别名称：${val}` : '未检测到详情页名称，请手动输入。', val ? 'ok' : 'err');
      };
    }
    if ($('clear-title')) {
      $('clear-title').onclick = () => {
        titleEdited = true;
        $('title').value = '';
        $('title').focus?.();
      };
    }
    const setFilter = filter => {
      activeFilter = filter;
      render();
    };
    if ($('filter-all')) $('filter-all').onclick = () => setFilter('all');
    if ($('filter-video')) $('filter-video').onclick = () => setFilter('video');
    if ($('filter-sub')) $('filter-sub').onclick = () => setFilter('sub');

    const appendBadge = (container, text, cls = '') => {
      const el = document.createElement('span');
      el.className = cls ? `badge ${cls}` : 'badge';
      el.textContent = text;
      container.append(el);
    };

    render = () => {
      const total = resources.size;
      let videoCount = 0;
      let subCount = 0;
      for (const item of resources.values()) {
        if (isSubtitleItem(item)) subCount += 1;
        else videoCount += 1;
      }
      toggleBtn.textContent = `下载 · ${total}`;
      if (toggleBtn.classList) toggleBtn.classList.toggle('has-media', total > 0);
      syncToggleVisibility();
      if ($('count-label')) $('count-label').textContent = `检测到 ${total} 个媒体`;
      if ($('filter-all')) { $('filter-all').textContent = `全部 (${total})`; $('filter-all').classList?.toggle('active', activeFilter === 'all'); }
      if ($('filter-video')) { $('filter-video').textContent = `视频 (${videoCount})`; $('filter-video').classList?.toggle('active', activeFilter === 'video'); }
      if ($('filter-sub')) { $('filter-sub').textContent = `字幕 (${subCount})`; $('filter-sub').classList?.toggle('active', activeFilter === 'sub'); }
      if ($('send') && !busy) {
        $('send').textContent = selected.size ? `发送所选 (${selected.size})` : '发送所选';
      }
      if (panel.hidden) return;
      const list = $('list');
      list.replaceChildren();
      const visible = getVisibleItems();
      if (!total) { list.textContent = '尚未发现媒体。开始播放，并开启需要的字幕；安装脚本后请刷新页面。'; return; }
      if (!visible.length) { list.textContent = '当前分类下暂无媒体，可切换「全部」查看。'; return; }
      for (const item of visible) {
        const row = document.createElement('div');
        row.className = 'resource';
        const checkbox = document.createElement('input');
        checkbox.type = 'checkbox';
        checkbox.checked = selected.has(item.url);
        checkbox.disabled = busy;
        checkbox.onchange = () => {
          if (checkbox.checked) selected.add(item.url);
          else selected.delete(item.url);
          if ($('send') && !busy) $('send').textContent = selected.size ? `发送所选 (${selected.size})` : '发送所选';
        };
        const text = document.createElement('span');
        text.textContent = item.type.includes('mpegurl') ? 'HLS / M3U8' : item.type;
        const badges = document.createElement('div');
        badges.className = 'badges';
        const meta = item.meta;
        if (meta?.isMaster) {
          appendBadge(badges, meta.variantsCount ? `主列表 · ${meta.variantsCount}档画质` : '主列表', 'badge-hd');
          if (meta.variantsSummary) appendBadge(badges, meta.variantsSummary, 'badge-info');
        } else if (meta?.resolution) {
          const resLabel = meta.height >= 2160 ? `4K (${meta.resolution})` : meta.height ? `${meta.height}p (${meta.resolution})` : meta.resolution;
          appendBadge(badges, resLabel, meta.height >= 720 ? 'badge-hd' : '');
        } else if (meta?.label) {
          appendBadge(badges, meta.inferred ? `${meta.label} (推测)` : meta.label, (meta.height || 0) >= 720 ? 'badge-hd' : '');
        }
        if (meta?.duration > 0) {
          const durText = formatDuration(meta.duration) + (meta.segmentCount ? ` · ${meta.segmentCount}片` : '');
          appendBadge(badges, durText);
        }
        if (meta?.bandwidth > 0) appendBadge(badges, formatBitrate(meta.bandwidth));
        if (meta?.encryption) appendBadge(badges, meta.encryption, 'badge-warn');
        const st = sendStates.get(item.url);
        if (st?.state === 'sending') appendBadge(badges, '发送中…', 'badge-info');
        else if (st?.state === 'sent') appendBadge(badges, '✓ 已发送', 'badge-ok');
        else if (st?.state === 'error') appendBadge(badges, '✕ 发送失败', 'badge-err');
        const address = document.createElement('small');
        const url = new URL(item.url);
        address.textContent = `${url.host}${url.pathname}`;
        if (badges.children.length) text.append(badges);
        text.append(address);
        text.onclick = () => {
          if (busy) return;
          checkbox.checked = !checkbox.checked;
          checkbox.onchange();
        };
        const quickBtn = document.createElement('button');
        quickBtn.type = 'button';
        quickBtn.className = st?.state === 'sent' ? 'row-send sent' : st?.state === 'error' ? 'row-send err' : 'row-send';
        quickBtn.disabled = busy;
        quickBtn.textContent = st?.state === 'sending' ? '发送中' : st?.state === 'sent' ? '重发' : '发送';
        quickBtn.onclick = e => {
          e?.stopPropagation?.();
          return sendItems([item]);
        };
        row.append(checkbox, text, quickBtn);
        list.append(row);
      }
    };
    $('all').onclick = () => {
      const visible = getVisibleItems();
      const allSelected = visible.length > 0 && visible.every(item => selected.has(item.url));
      if (allSelected) {
        for (const item of visible) selected.delete(item.url);
      } else {
        for (const item of visible) selected.add(item.url);
      }
      render();
    };
    $('refresh').onclick = () => {
      scan();
      syncPageContext(false);
      render();
    };
    if ($('probe')) {
      $('probe').onclick = async () => {
        if (busy || probing) return;
        const candidates = [...resources.values()].filter(item => item.type.includes('mpegurl') && !item.meta?.isMaster && !item.meta?.duration).slice(0, 12);
        if (!candidates.length) {
          setStatus('当前列表中的 M3U8 已识别清晰度或时长信息。', 'info');
          return;
        }
        probing = true;
        $('probe').disabled = true;
        $('probe').textContent = '探测中…';
        setStatus(`正在探测 ${candidates.length} 个 M3U8 清单信息…`, 'info');
        let okCount = 0;
        const beforeSize = resources.size;
        const fetchPlaylist = item => new Promise(resolve => {
          const headers = {};
          if (item.headers?.referer) headers.Referer = item.headers.referer;
          if (item.headers?.origin) headers.Origin = item.headers.origin;
          GM_xmlhttpRequest({
            method: 'GET',
            url: item.url,
            headers,
            anonymous: !item.cookiesAllowed,
            timeout: 10000,
            onload: response => {
              const text = String(response.responseText || '').slice(0, maxPlaylistBytes);
              if (response.status >= 200 && response.status < 300 && text.trimStart().startsWith('#EXTM3U')) {
                ingestM3u8Text(item.url, text, item.headers, item.cookiesAllowed);
                okCount += 1;
              }
              resolve();
            },
            onerror: () => resolve(),
            ontimeout: () => resolve(),
          });
        });
        try {
          const queue = [...candidates];
          const workers = Array.from({ length: Math.min(3, queue.length) }, async () => {
            while (queue.length) {
              const next = queue.shift();
              if (next) await fetchPlaylist(next);
            }
          });
          await Promise.all(workers);
          const addedVariants = Math.max(0, resources.size - beforeSize);
          render();
          if (okCount > 0) {
            setStatus(`已探测 ${okCount} 个 M3U8${addedVariants ? `，发现 ${addedVariants} 个清晰度分支` : ''}。`, 'ok');
          } else {
            setStatus('未能读取到清单内容，可能需先点击播放或来源站拒绝跨域读取。', 'err');
          }
        } finally {
          probing = false;
          $('probe').disabled = false;
          $('probe').textContent = '探测清晰度';
        }
      };
    }
    $('manual-add').onclick = () => {
      if (busy) return;
      const url = httpUrl($('manual-url').value.trim());
      if (!url) { setStatus('请粘贴以 http:// 或 https:// 开头的媒体直链。', 'err'); return; }
      if (!resources.has(url) && resources.size >= 200) { setStatus('资源列表已满，请刷新页面后重试。', 'err'); return; }
      record(url, mediaType(url) || 'application/vnd.apple.mpegurl');
      selected.add(url);
      $('manual-url').value = '';
      render();
      setStatus('已添加并选中媒体地址。请确认播放页来源后发送。', 'ok');
    };
    function readCookies(url) {
      return new Promise((resolve, reject) => {
        const timer = typeof setTimeout === 'function'
          ? setTimeout(() => reject(new Error('Cookie 授权超时，请在脚本猫中允许读取媒体域 Cookie 后重试。')), 30000)
          : null;
        timer?.unref?.();
        try {
          GM_cookie('list', { url }, (cookies, error) => {
            if (timer && typeof clearTimeout === 'function') clearTimeout(timer);
            if (error) reject(new Error('未取得媒体域 Cookie 授权，请允许后重试，或取消勾选 Cookie。'));
            else resolve(cookieHeader(cookies || [], url));
          });
        } catch (error) {
          if (timer && typeof clearTimeout === 'function') clearTimeout(timer);
          reject(error);
        }
      });
    }
    const controlIds = ['send', 'all', 'refresh', 'probe', 'resync-title', 'clear-title'];
    async function sendItems(chosen) {
      if (busy) return;
      const server = httpUrl($('server').value.trim());
      const secret = $('secret').value.trim();
      const source = httpUrl($('source').value.trim());
      if (!server || !secret) {
        if ($('settings')) $('settings').open = true;
        setStatus('请先填写下载器地址和访问密码 (DS_SECRET)。', 'err', true);
        return;
      }
      if (!source) {
        if ($('advanced')) $('advanced').open = true;
        setStatus('请填写有效的详情页地址。', 'err', true);
        return;
      }
      if (!chosen.length || chosen.length > 100) {
        setStatus('每次请选择 1–100 个媒体或字幕，或点击单条右侧「发送」。', 'err', true);
        return;
      }
      busy = true;
      for (const item of chosen) sendStates.set(item.url, { state: 'sending' });
      for (const id of controlIds) if ($(id)) $(id).disabled = true;
      if ($('open-dl')) $('open-dl').hidden = true;
      if ($('send')) $('send').textContent = '正在发送…';
      render();
      try {
        const title = $('title').value;
        const includeCookies = $('cookies').checked;
        const data = [];
        status('正在读取选中媒体的请求信息…');
        scanTokenSession();
        for (const item of chosen) {
          let withSession = attachTokenSessionIfEligible(item);
          const headers = { ...withSession.headers };
          delete headers.cookie;
          const cookies = includeCookies && withSession.cookiesAllowed ? await readCookies(withSession.url) : '';
          if (includeCookies && withSession.tokenSession?.playerUrl && !withSession.tokenSession.cookie) {
            try {
              const playerCookies = await readCookies(withSession.tokenSession.playerUrl);
              if (playerCookies) {
                withSession = { ...withSession, tokenSession: { ...withSession.tokenSession, cookie: playerCookies } };
              }
            } catch { /* Ignore if player-origin cookies are unavailable or not authorized. */ }
          }
          data.push(payloadFor({ ...withSession, headers }, title, source, cookies));
        }
        const endpoint = new URL(server);
        endpoint.pathname = `${endpoint.pathname.replace(/\/$/, '').replace(/\/api\/cat-catch$/, '')}/api/cat-catch`;
        endpoint.search = '';
        const cleanServer = server.replace(/\/$/, '');
        status('正在发送到 Docker…');
        const result = await new Promise((resolve, reject) => GM_xmlhttpRequest({
          method: 'POST', url: endpoint.href, anonymous: true, timeout: 30000,
          headers: { 'Content-Type': 'application/json', Authorization: secret },
          data: JSON.stringify({ action: 'catch', data }),
          onload: response => {
            const parsed = parseApiResponse(response.status, response.responseText);
            if (!parsed.ok) reject(new Error(parsed.message));
            else resolve(parsed.data);
          },
          onerror: () => reject(new Error('无法连接下载器，请检查地址、端口及脚本猫连接授权。')),
          ontimeout: () => reject(new Error('下载器响应超时，请检查队列后再重试。')),
        }));
        for (const item of chosen) sendStates.set(item.url, { state: 'sent' });
        if ($('open-dl')) {
          $('open-dl').href = cleanServer;
          $('open-dl').hidden = false;
        }
        const msg = `已加入 ${result.added ?? 0} 项，跳过 ${result.skipped ?? 0} 项。可到下载器查看进度。`;
        setStatus(msg, 'ok', true);
      } catch (error) {
        for (const item of chosen) sendStates.set(item.url, { state: 'error' });
        setStatus(error?.message || '发送失败，请检查网络连接或下载器配置。', 'err', true);
      } finally {
        busy = false;
        for (const id of controlIds) if ($(id)) $(id).disabled = false;
        render();
      }
    }
    $('send').onclick = () => {
      const chosen = [...selected].map(url => resources.get(url)).filter(Boolean);
      return sendItems(chosen);
    };
    render();
    scan();
  }
  if (document.documentElement) mount();
  else document.addEventListener('DOMContentLoaded', mount, { once: true });
})();
