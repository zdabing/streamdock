const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const {
  httpUrl,
  mediaType,
  scopedHeaders,
  referrerFor,
  cookieHeader,
  payloadFor,
  extractTokenSessionFromText,
  applyFreshTokenToUrl,
} = require('../examples/scriptcat-downloader.user.js');

test('ScriptCat preserves signed queries, titles and separate page/media credential identities', () => {
  const url = 'https://cdn.example/a/video.m3u8?token=a%2Bb&expires=9999999999&token_path=%2Fa%2F';
  assert.equal(httpUrl(url), url);
  assert.equal(httpUrl('javascript:alert(1)'), '');
  assert.equal(httpUrl('https://user:pass@example.com/a'), '');
  const item = {
    url,
    type: mediaType(url),
    headers: { origin: 'https://page.example', referer: 'https://page.example/', authorization: 'media-token' },
  };
  const payload = payloadFor(item, '作品 / 第1集', 'https://page.example/detail/1', 'cdn=session');
  assert.equal(payload.name, '作品 _ 第1集.mp4');
  assert.equal(payload.url, url);
  assert.equal(payload.sourceUrl, 'https://page.example/detail/1');
  assert.equal(payload.requestHeaders.cookie, 'cdn=session');
  assert.equal(payload.requestHeaders.authorization, 'media-token');
  assert.equal(
    payloadFor({ ...item, url: 'https://cdn.example/zh.vtt', type: 'text/vtt' }, '作品', payload.sourceUrl, '').name,
    '作品.zh.vtt'
  );
  assert.equal(mediaType('https://cdn.example/index.txt', 'application/x-mpegURL'), 'application/vnd.apple.mpegurl');
  assert.equal(mediaType('https://cdn.example/video.ts'), '');
  assert.deepEqual(
    scopedHeaders(
      { Cookie: 'secret', Authorization: 'secret', Origin: 'https://page.example', Host: 'bad' },
      url,
      'https://other.example/a'
    ),
    { origin: 'https://page.example' }
  );
  assert.equal(referrerFor('https://page.example/detail/1', url), 'https://page.example/');
  assert.equal(referrerFor('https://page.example/detail/1', 'https://page.example/a'), 'https://page.example/detail/1');
  assert.equal(referrerFor('https://page.example/detail/1', url, 'no-referrer'), '');
});

test('ScriptCat cookie selection scopes HttpOnly cookies to media host, path, scheme and expiry', () => {
  const cookie = { name: 'session', value: 'media', domain: '.cdn.example', path: '/a', httpOnly: true, secure: true };
  const cookies = [
    cookie,
    { ...cookie, domain: 'page.example', name: 'page' },
    { ...cookie, path: '/ab', name: 'wrongpath' },
    { ...cookie, hostOnly: true, name: 'hostonly' },
    { ...cookie, expirationDate: 1, name: 'expired' },
    { ...cookie, name: 'specific', path: '/a/b' },
  ];
  assert.equal(cookieHeader(cookies, 'https://sub.cdn.example/a/b/1.m3u8'), 'specific=media; session=media');
  assert.equal(cookieHeader(cookies, 'http://sub.cdn.example/a/b/1.m3u8'), '');
  assert.equal(cookieHeader(cookies, 'https://other.example/a/b/1.m3u8'), '');
});

test('ScriptCat recognizes extensionless object storage links without altering signatures', () => {
  const url =
    'https://storage.example/8895809daf474dc68854afd2ff38ca8e086?response-content-disposition=attachment%3B%20filename%2A%3DUTF-8%27%27Love.in.Disguise.S01E03.mp4&X-Amz-Signature=a%2Bb&X-Amz-Expires=900';
  assert.equal(mediaType(url), 'video/mp4');
  const payload = payloadFor({ url, type: mediaType(url), headers: {} }, '第3集', 'https://page.example/watch', '');
  assert.equal(payload.url, url);
  assert.equal(payload.name, '第3集.mp4');
  assert.equal(mediaType('https://storage.example/object', 'application/octet-stream', 'attachment; filename="字幕.vtt"'), 'text/vtt');
  assert.equal(mediaType('https://storage.example/object?response-content-type=video%2Fmp4'), 'video/mp4');
  assert.equal(mediaType('https://storage.example/object?filename=video.mp4'), 'video/mp4');
  assert.equal(mediaType('https://storage.example/object?random=video.mp4'), '');
  assert.equal(
    mediaType('https://storage.example/object', '', "attachment; filename*=UTF-8''%E4%B8%AD%E6%96%87.srt"),
    'application/x-subrip'
  );
});

test('ScriptCat recognizes audio extensions, MIME aliases and attachment names and preserves audio filenames', () => {
  const samples = [
    ['mp3', 'audio/mpeg', 'audio/mp3'],
    ['m4a', 'audio/mp4', 'audio/x-m4a'],
    ['aac', 'audio/aac', 'audio/aacp'],
    ['wav', 'audio/wav', 'audio/x-wav'],
    ['flac', 'audio/flac', 'audio/x-flac'],
    ['ogg', 'audio/ogg', 'application/ogg'],
    ['opus', 'audio/opus', 'audio/opus'],
    ['weba', 'audio/webm', 'audio/webm'],
  ];
  for (const [ext, mime, alias] of samples) {
    assert.equal(mediaType(`https://cdn.example/song.${ext}?token=a%2Bb`), mime);
    assert.equal(mediaType('https://cdn.example/object', `${alias}; charset=utf-8`), mime);
    assert.equal(mediaType('https://cdn.example/object', 'application/octet-stream', `attachment; filename="song.${ext}"`), mime);
    const url = `https://cdn.example/object?filename=song.${ext}&token=a%2Bb`;
    const data = payloadFor({ url, type: mediaType(url), headers: {} }, '歌曲名称', 'https://page.example/album', '');
    assert.equal(data.name, `歌曲名称.${ext}`);
    assert.equal(data.url, url);
  }
  assert.equal(mediaType('https://cdn.example/song.opus', 'audio/ogg'), 'audio/opus');
  assert.equal(mediaType('https://cdn.example/a.ts', 'video/mp2t'), '');
  assert.equal(mediaType('https://cdn.example/a.mpd', 'application/dash+xml'), '');
});

test('ScriptCat observes real fetch/XHR response objects without consuming page responses', async () => {
  const messages = [];
  class XHR {
    open() {}
    setRequestHeader() {}
    send() {
      this.responseURL = 'https://cdn.example/subtitles?id=1';
      this.responseType = '';
      this.responseText = 'WEBVTT';
      this.listener();
    }
    getResponseHeader() {
      return 'text/vtt';
    }
    addEventListener(_name, fn) {
      this.listener = fn;
    }
  }
  const page = {
    XMLHttpRequest: XHR,
    fetch: async () => new Response('#EXTM3U\n#EXTINF:1,\na.ts', { headers: { 'content-type': 'text/plain' } }),
  };
  const sandbox = {
    unsafeWindow: page,
    window: { top: { postMessage: value => messages.push(value) } },
    location: new URL('https://page.example/player'),
    document: { referrerPolicy: '', addEventListener() {} },
    navigator: { userAgent: 'fixture browser' },
    URL,
    Headers,
    TextDecoder,
    setInterval() {},
  };
  vm.runInNewContext(fs.readFileSync(require.resolve('../examples/scriptcat-downloader.user.js'), 'utf8'), sandbox);
  const response = await page.fetch('https://cdn.example/index.txt', { credentials: 'include', headers: { Authorization: 'signed' } });
  assert.match(await response.text(), /^#EXTM3U/);
  for (let i = 0; i < 20 && !messages.length; i++) await new Promise(resolve => setTimeout(resolve, 10));
  assert.equal(messages[0].item.url, 'https://cdn.example/index.txt');
  assert.equal(messages[0].item.cookiesAllowed, true);
  assert.equal(messages[0].item.headers.authorization, 'signed');
  assert.equal(messages[0].item.headers.origin, 'https://page.example');
  assert.equal(messages[0].item.headers.referer, 'https://page.example/');
  await page.fetch('https://cdn.example/omit.m3u8', { credentials: 'omit' });
  assert.equal(messages.find(message => message.item.url.endsWith('/omit.m3u8')).item.cookiesAllowed, false);
  const xhr = new XHR();
  xhr.open('GET', 'https://cdn.example/subtitles?id=1');
  xhr.withCredentials = true;
  xhr.setRequestHeader('Authorization', 'subtitle-token');
  xhr.send();
  assert.equal(messages.at(-1).item.type, 'text/vtt');
  assert.equal(messages.at(-1).item.headers.authorization, 'subtitle-token');
});

test('ScriptCat panel sends selected media only, reads target cookies on click and keeps Docker auth separate', async () => {
  class Element {
    constructor() {
      this.children = [];
      this.style = {};
      this.value = '';
    }
    append(...children) {
      this.children.push(...children);
    }
    replaceChildren() {
      this.children = [];
    }
  }
  const fields = new Map();
  const panel = new Element();
  panel.hidden = true;
  const root = {
    getElementById(id) {
      if (!fields.has(id)) fields.set(id, new Element());
      return fields.get(id);
    },
    querySelector: () => panel,
  };
  root.getElementById('cookies').checked = true;
  const element = () => Object.assign(new Element(), { attachShadow: () => root });
  const requests = [];
  const cookieReads = [];
  const historical = [];
  class XHR {
    open() {}
    setRequestHeader() {}
    send() {}
  }
  const page = {
    XMLHttpRequest: XHR,
    fetch: async () => new Response('', { headers: { 'content-type': 'application/vnd.apple.mpegurl' } }),
  };
  const window = { addEventListener() {} };
  window.top = window;
  const sandbox = {
    unsafeWindow: page,
    window,
    location: new URL('https://page.example/detail/1'),
    document: {
      title: '作品名称',
      documentElement: new Element(),
      createElement: element,
      referrerPolicy: '',
      addEventListener() {},
      querySelectorAll: () => [],
    },
    navigator: { userAgent: 'fixture browser' },
    URL,
    Headers,
    TextDecoder,
    setInterval() {},
    setTimeout,
    clearTimeout,
    performance: { getEntriesByType: () => historical },
    GM_getValue: () => ({ server: 'https://docker.example/prefix', secret: 'docker-secret' }),
    GM_setValue() {},
    GM_registerMenuCommand() {},
    GM_cookie(action, details, callback) {
      cookieReads.push({ action, details });
      callback([{ name: 'session', value: 'media-only', domain: 'cdn.example', path: '/', hostOnly: true, httpOnly: true }]);
    },
    GM_xmlhttpRequest(details) {
      requests.push(details);
      details.onload({ status: 200, responseText: JSON.stringify({ code: 0, data: { added: 1, skipped: 0 } }) });
    },
  };
  vm.runInNewContext(fs.readFileSync(require.resolve('../examples/scriptcat-downloader.user.js'), 'utf8'), sandbox);
  await page.fetch('https://cdn.example/video.m3u8?token=abc', { credentials: 'include', headers: { Authorization: 'media-secret' } });
  await page.fetch('https://cdn.example/other.m3u8?token=def', { credentials: 'include' });
  assert.equal(cookieReads.length, 0);
  assert.equal(requests.length, 0);
  root.getElementById('toggle').onclick();
  assert.equal(root.getElementById('title').value, '作品名称');
  const checkbox = root.getElementById('list').children[0].children[0];
  checkbox.checked = true;
  checkbox.onchange();
  await root.getElementById('send').onclick();
  assert.equal(cookieReads.length, 1);
  assert.equal(cookieReads[0].details.url, 'https://cdn.example/video.m3u8?token=abc');
  assert.equal(requests.length, 1);
  const sent = requests[0];
  assert.equal(sent.url, 'https://docker.example/prefix/api/cat-catch');
  assert.equal(sent.headers.Authorization, 'docker-secret');
  assert.equal(sent.anonymous, true);
  const body = JSON.parse(sent.data);
  assert.equal(body.data.length, 1);
  assert.equal(body.data[0].name, '作品名称.mp4');
  assert.equal(body.data[0].sourceUrl, 'https://page.example/detail/1');
  assert.equal(body.data[0].requestHeaders.cookie, 'session=media-only');
  assert.equal(body.data[0].requestHeaders.authorization, 'media-secret');
  assert.match(root.getElementById('status').textContent, /已加入 1 项/);
  historical.push({ name: 'https://cdn.example/object?response-content-disposition=attachment%3Bfilename%3Dvideo.mp4' });
  root.getElementById('refresh').onclick();
  assert.equal(root.getElementById('list').children.length, 3);
  assert.equal(root.getElementById('list').children[2].children[1].textContent, 'video/mp4');
  const oldCheckbox = root.getElementById('list').children[0].children[0];
  oldCheckbox.checked = false;
  oldCheckbox.onchange();
  root.getElementById('manual-url').value = 'javascript:alert(1)';
  root.getElementById('manual-add').onclick();
  assert.equal(root.getElementById('list').children.length, 3);
  root.getElementById('manual-url').value = 'https://cdn.example/manual?id=1&token=a%2Bb';
  root.getElementById('manual-add').onclick();
  assert.equal(root.getElementById('list').children.length, 4);
  assert.equal(root.getElementById('list').children[3].children[0].checked, true);
  await root.getElementById('send').onclick();
  const manual = JSON.parse(requests[1].data).data;
  assert.equal(manual.length, 1);
  assert.equal(manual[0].url, 'https://cdn.example/manual?id=1&token=a%2Bb');
  assert.equal(manual[0].type, 'application/vnd.apple.mpegurl');
});

test('ScriptCat hides floating button on configured WebUI origin and restores saved vertical offset', () => {
  class Element {
    constructor() {
      this.children = [];
      this.style = {};
      this.value = '';
    }
    append(...children) {
      this.children.push(...children);
    }
    replaceChildren() {
      this.children = [];
    }
  }
  const fields = new Map();
  const panel = new Element();
  panel.hidden = true;
  const root = {
    getElementById(id) {
      if (!fields.has(id)) fields.set(id, new Element());
      return fields.get(id);
    },
    querySelector: () => panel,
  };
  const element = () => Object.assign(new Element(), { attachShadow: () => root });
  class XHR {
    open() {}
    setRequestHeader() {}
    send() {}
  }
  const window = { innerHeight: 800, addEventListener() {} };
  window.top = window;
  const sandbox = {
    unsafeWindow: { XMLHttpRequest: XHR },
    window,
    location: new URL('http://10.0.0.194:6600/'),
    document: {
      title: 'M3U8',
      documentElement: new Element(),
      createElement: element,
      referrerPolicy: '',
      addEventListener() {},
      querySelectorAll: () => [],
    },
    navigator: { userAgent: 'fixture' },
    URL,
    Headers,
    TextDecoder,
    setInterval() {},
    setTimeout,
    clearTimeout,
    GM_getValue: key => (key === 'docker-btn-bottom' ? 180 : { server: 'http://10.0.0.194:6600', secret: '123456' }),
    GM_setValue() {},
    GM_registerMenuCommand() {},
  };
  vm.runInNewContext(fs.readFileSync(require.resolve('../examples/scriptcat-downloader.user.js'), 'utf8'), sandbox);
  assert.equal(root.getElementById('toggle').hidden, true);
  assert.equal(root.getElementById('toggle').style.bottom, '180px');
});

test('ScriptCat extracts detail titles over generic titles and parses M3U8 metadata and API errors', () => {
  const {
    extractPageTitle,
    inferQualityFromUrl,
    formatDuration,
    formatBitrate,
    parseM3u8Meta,
    parseApiResponse,
  } = require('../examples/scriptcat-downloader.user.js');

  const ldDoc = {
    title: '播放器 - 加载中...',
    querySelectorAll: sel =>
      sel.includes('ld+json') ? [{ textContent: JSON.stringify({ '@graph': [{ '@type': 'TVEpisode', name: '绝命毒师 S01E01' }] }) }] : [],
    querySelector: sel => (sel.includes('og:title') ? { getAttribute: () => '错误兜底标题' } : null),
  };
  assert.equal(extractPageTitle(ldDoc, 'https://site.example/play/1'), '绝命毒师 S01E01');

  const h1Doc = {
    title: 'Loading...',
    querySelectorAll: () => [],
    querySelector: sel => (sel === 'h1.video-title' ? { textContent: '银河护卫队 - 免费在线观看 - example.tv' } : null),
  };
  assert.equal(extractPageTitle(h1Doc, 'https://www.example.tv/v/42'), '银河护卫队');

  const hyphenDoc = {
    title: 'SSIS-123 特别篇 - 免费在线观看 - example.tv',
    querySelectorAll: () => [],
    querySelector: () => null,
  };
  assert.equal(extractPageTitle(hyphenDoc, 'https://www.example.tv/v/43'), 'SSIS-123 特别篇');

  const genericOnlyDoc = {
    title: '正在加载...',
    querySelectorAll: () => [],
    querySelector: () => null,
  };
  assert.equal(extractPageTitle(genericOnlyDoc, 'https://site.example/v/1'), '');

  assert.deepEqual(inferQualityFromUrl('https://cdn.example/vod/1080p/index.m3u8'), { height: 1080, label: '1080p', inferred: true });
  assert.deepEqual(inferQualityFromUrl('https://cdn.example/v_1920x1080.m3u8'), {
    width: 1920,
    height: 1080,
    resolution: '1920x1080',
    label: '1080p',
    inferred: true,
  });
  assert.equal(formatDuration(2108), '35:08');
  assert.equal(formatDuration(3912), '1:05:12');
  assert.equal(formatBitrate(4_500_000), '4.5 Mbps');
  assert.equal(formatBitrate(850_000), '850 kbps');

  const masterMeta = parseM3u8Meta(
    [
      '#EXTM3U',
      '#EXT-X-STREAM-INF:BANDWIDTH=2200000,RESOLUTION=1280x720',
      '720p/video.m3u8',
      '#EXT-X-STREAM-INF:BANDWIDTH=4500000,RESOLUTION=1920x1080',
      'https://other.cdn/1080p/video.m3u8',
    ].join('\n'),
    'https://cdn.example/stream/master.m3u8'
  );
  assert.equal(masterMeta.isMaster, true);
  assert.equal(masterMeta.variants.length, 2);
  assert.equal(masterMeta.variants[0].height, 1080);
  assert.equal(masterMeta.variants[0].url, 'https://other.cdn/1080p/video.m3u8');
  assert.equal(masterMeta.variants[1].url, 'https://cdn.example/stream/720p/video.m3u8');

  const mediaMeta = parseM3u8Meta(
    ['#EXTM3U', '#EXT-X-KEY:METHOD=AES-128,URI="key.key"', '#EXTINF:100.4,', 'seg1.ts', '#EXTINF:110.0,', 'seg2.ts', '#EXT-X-ENDLIST'].join(
      '\n'
    ),
    'https://cdn.example/720p/video.m3u8'
  );
  assert.equal(mediaMeta.isMaster, false);
  assert.equal(mediaMeta.duration, 210);
  assert.equal(mediaMeta.segmentCount, 2);
  assert.equal(mediaMeta.encryption, 'AES-128');

  assert.deepEqual(parseApiResponse(200, JSON.stringify({ code: 0, data: { added: 1, skipped: 0 } })), {
    ok: true,
    data: { added: 1, skipped: 0 },
    message: '',
  });
  assert.match(parseApiResponse(401, '<html>Unauthorized</html>').message, /DS_SECRET|401/);
  assert.match(parseApiResponse(502, '<html>Bad Gateway</html>').message, /502/);
});

test('ScriptCat extracts variants without cross-origin credential leakage, filters, probes and reports send results', async () => {
  class Element {
    constructor() {
      this.children = [];
      this.style = {};
      this.value = '';
      this.hidden = false;
      this.className = '';
    }
    append(...children) {
      this.children.push(...children);
    }
    replaceChildren() {
      this.children = [];
    }
  }
  const fields = new Map();
  const panel = new Element();
  panel.hidden = true;
  const root = {
    getElementById(id) {
      if (!fields.has(id)) fields.set(id, new Element());
      return fields.get(id);
    },
    querySelector: () => panel,
  };
  let currentH1 = '第一集 真正片名 - 免费在线观看';
  const createEl = () => Object.assign(new Element(), { attachShadow: () => root });
  const listeners = new Map();
  const requests = [];
  const cookieReads = [];
  let nextStatus = 200;
  let nextBody = JSON.stringify({ code: 0, data: { added: 1, skipped: 0 } });
  const masterBody = [
    '#EXTM3U',
    '#EXT-X-STREAM-INF:BANDWIDTH=4500000,RESOLUTION=1920x1080',
    '/hd/1080p.m3u8',
    '#EXT-X-STREAM-INF:BANDWIDTH=1200000,RESOLUTION=854x480',
    'https://other-cdn.example/sd/480p.m3u8',
  ].join('\n');
  const probedMediaBody = [
    '#EXTM3U',
    '#EXT-X-KEY:METHOD=AES-128,URI="enc.key"',
    '#EXTINF:1200,',
    'part1.ts',
    '#EXTINF:908,',
    'part2.ts',
    '#EXT-X-ENDLIST',
  ].join('\n');

  const page = {
    fetch: async url => {
      const u = String(url);
      if (u.endsWith('master.m3u8')) {
        return new Response(masterBody, { status: 200, headers: { 'Content-Type': 'application/vnd.apple.mpegurl' } });
      }
      if (u.endsWith('.vtt')) {
        return new Response('WEBVTT\n', { status: 200, headers: { 'Content-Type': 'text/vtt' } });
      }
      return new Response('', { status: 200 });
    },
    XMLHttpRequest: class XHR {
      open() {}
      setRequestHeader() {}
      send() {}
    },
  };
  const location = { href: 'https://site.example/play/1', origin: 'https://site.example' };
  const window = { innerHeight: 844, addEventListener: (name, fn) => listeners.set(name, fn) };
  window.top = window;
  const document = {
    title: '播放器 - 加载中',
    documentElement: new Element(),
    createElement: createEl,
    referrerPolicy: 'origin',
    addEventListener() {},
    querySelector: sel => (sel === 'h1.video-title' ? { textContent: currentH1 } : null),
    querySelectorAll: () => [],
  };
  const sandbox = {
    unsafeWindow: page,
    window,
    location,
    document,
    navigator: { userAgent: 'mobile-ua' },
    URL,
    Headers,
    TextDecoder,
    setInterval: () => 0,
    setTimeout,
    clearTimeout,
    GM_getValue: key => (key === 'docker-settings' ? { server: 'http://docker.local:6600', secret: 'good-secret' } : null),
    GM_setValue() {},
    GM_registerMenuCommand() {},
    GM_cookie(_action, details, cb) {
      cookieReads.push(details.url);
      cb([{ domain: new URL(details.url).hostname, path: '/', name: 'sid', value: 's1', secure: true }], null);
    },
    GM_xmlhttpRequest(opts) {
      if (opts.method === 'GET') {
        opts.onload({
          status: opts.url.includes('1080p.m3u8') ? 200 : 404,
          responseText: opts.url.includes('1080p.m3u8') ? probedMediaBody : '',
        });
        return;
      }
      requests.push(opts);
      opts.onload({ status: nextStatus, responseText: nextBody });
    },
  };
  vm.runInNewContext(fs.readFileSync(require.resolve('../examples/scriptcat-downloader.user.js'), 'utf8'), sandbox);
  root.getElementById('cookies').checked = true;

  await page.fetch('https://cdn.example/master.m3u8', {
    credentials: 'include',
    headers: { Authorization: 'media-token' },
  });
  await page.fetch('https://cdn.example/zh.vtt');
  await new Promise(resolve => setImmediate(resolve));

  root.getElementById('toggle').onclick();
  assert.equal(root.getElementById('title').value, '第一集 真正片名');

  // SPA navigation should auto-refresh title and source until user edits manually.
  location.href = 'https://site.example/play/2';
  currentH1 = '第二集 更新标题';
  root.getElementById('refresh').onclick();
  assert.equal(root.getElementById('title').value, '第二集 更新标题');
  assert.equal(root.getElementById('source').value, 'https://site.example/play/2');

  root.getElementById('title').value = '手动命名';
  root.getElementById('title').oninput();
  currentH1 = '不应覆盖';
  root.getElementById('refresh').onclick();
  assert.equal(root.getElementById('title').value, '手动命名');
  root.getElementById('resync-title').onclick();
  assert.equal(root.getElementById('title').value, '不应覆盖');

  // Filter checks: all -> sub -> video -> all.
  assert.equal(root.getElementById('list').children.length, 4);
  root.getElementById('filter-sub').onclick();
  assert.equal(root.getElementById('list').children.length, 1);
  assert.equal(root.getElementById('list').children[0].children[1].children[0].textContent, 'cdn.example/zh.vtt');
  root.getElementById('filter-video').onclick();
  assert.equal(root.getElementById('list').children.length, 3);
  root.getElementById('filter-all').onclick();
  assert.equal(root.getElementById('list').children.length, 4);

  // Explicit probe populates duration and encryption badges on 1080p variant.
  await root.getElementById('probe').onclick();
  const findRow = suffix =>
    root
      .getElementById('list')
      .children.find(r => r.children[1].children.some(c => typeof c.textContent === 'string' && c.textContent.includes(suffix)));
  const hdRow = findRow('/hd/1080p.m3u8');
  assert.ok(hdRow);
  const hdBadges = hdRow.children[1].children[0].children.map(el => el.textContent);
  assert.ok(hdBadges.some(t => t.includes('1080p (1920x1080)')));
  assert.ok(hdBadges.some(t => t.includes('35:08')));
  assert.ok(hdBadges.some(t => t.includes('4.5 Mbps')));
  assert.ok(hdBadges.some(t => t.includes('AES-128')));

  // 1-tap send on same-origin 1080p row includes media credentials and shows ok feedback.
  await hdRow.children[2].onclick();
  assert.equal(cookieReads.length, 1);
  const sameOriginSent = JSON.parse(requests[0].data).data[0];
  assert.equal(sameOriginSent.requestHeaders.authorization, 'media-token');
  assert.equal(sameOriginSent.requestHeaders.cookie, 'sid=s1');
  assert.equal(root.getElementById('status').className, 'status-ok');
  assert.equal(root.getElementById('toast').className, 'toast-ok');
  assert.equal(root.getElementById('toast').hidden, false);
  assert.equal(root.getElementById('open-dl').hidden, false);

  // 1-tap send on cross-origin 480p variant strips credentials & skips cookies; 401 failure shows error feedback.
  nextStatus = 401;
  nextBody = '<html>Unauthorized</html>';
  const crossRow = findRow('other-cdn.example/sd/480p.m3u8');
  assert.ok(crossRow);
  await crossRow.children[2].onclick();
  assert.equal(cookieReads.length, 1);
  const crossSent = JSON.parse(requests[1].data).data[0];
  assert.equal(crossSent.requestHeaders.authorization, undefined);
  assert.equal(crossSent.requestHeaders.cookie, undefined);
  assert.equal(root.getElementById('status').className, 'status-err');
  assert.equal(root.getElementById('toast').className, 'toast-err');
  assert.match(root.getElementById('status').textContent, /401|DS_SECRET/);
  assert.equal(root.getElementById('open-dl').hidden, true);

  // Subsequent direct authenticated fetch of that cross-origin variant upgrades cookiesAllowed cleanly.
  nextStatus = 200;
  nextBody = JSON.stringify({ code: 0, data: { added: 1, skipped: 0 } });
  await page.fetch('https://other-cdn.example/sd/480p.m3u8', { credentials: 'include' });
  await new Promise(resolve => setImmediate(resolve));
  const upgradedRow = findRow('other-cdn.example/sd/480p.m3u8');
  assert.ok(upgradedRow);
  await upgradedRow.children[2].onclick();
  assert.equal(cookieReads.length, 2);
  assert.equal(cookieReads[1], 'https://other-cdn.example/sd/480p.m3u8');
  assert.equal(JSON.parse(requests[2].data).data[0].requestHeaders.cookie, 'sid=s1');
});

test('ScriptCat auto-hides empty floating button by default, reveals on media detection, and persists hideWhenEmpty option', async () => {
  class Element {
    constructor() {
      this.children = [];
      this.style = {};
      this.value = '';
      this.hidden = false;
      this.className = '';
    }
    append(...children) {
      this.children.push(...children);
    }
    replaceChildren() {
      this.children = [];
    }
  }
  const createSandbox = (pageUrl, initialStore) => {
    const fields = new Map();
    const panel = new Element();
    panel.hidden = true;
    const root = {
      getElementById(id) {
        if (!fields.has(id)) fields.set(id, new Element());
        return fields.get(id);
      },
      querySelector: () => panel,
    };
    let menuCallback = null;
    let stored = initialStore;
    const savedCalls = [];
    const page = {
      fetch: async () =>
        new Response('#EXTM3U\n#EXTINF:5,\nseg.ts', { status: 200, headers: { 'Content-Type': 'application/vnd.apple.mpegurl' } }),
      XMLHttpRequest: class XHR {
        open() {}
        setRequestHeader() {}
        send() {}
      },
    };
    const window = { innerHeight: 844, addEventListener() {} };
    window.top = window;
    const sandbox = {
      unsafeWindow: page,
      window,
      location: new URL(pageUrl),
      document: {
        title: '普通新闻页面',
        documentElement: new Element(),
        createElement: () => Object.assign(new Element(), { attachShadow: () => root }),
        referrerPolicy: '',
        addEventListener() {},
        querySelector: () => null,
        querySelectorAll: () => [],
      },
      navigator: { userAgent: 'fixture' },
      URL,
      Headers,
      TextDecoder,
      setInterval: () => 0,
      setTimeout,
      clearTimeout,
      GM_getValue: key => (key === 'docker-settings' ? stored : null),
      GM_setValue: (key, val) => {
        if (key === 'docker-settings') {
          stored = val;
          savedCalls.push(val);
        }
      },
      GM_registerMenuCommand: (_name, fn) => {
        menuCallback = fn;
      },
    };
    vm.runInNewContext(fs.readFileSync(require.resolve('../examples/scriptcat-downloader.user.js'), 'utf8'), sandbox);
    return { root, panel, page, getMenu: () => menuCallback, getStored: () => stored, savedCalls };
  };

  // 1. Default behavior on non-WebUI page: 0 media -> #toggle hidden, menu command still opens panel
  const normal = createSandbox('https://news.example/article/1', { server: 'http://docker.local:6600', secret: 'sec' });
  const toggle = normal.root.getElementById('toggle');
  const hideEmpty = normal.root.getElementById('hide-empty');
  assert.equal(hideEmpty.checked, true);
  assert.equal(toggle.hidden, true);

  normal.getMenu()();
  assert.equal(normal.panel.hidden, false);
  assert.equal(toggle.hidden, true);
  normal.root.getElementById('close').onclick();
  assert.equal(normal.panel.hidden, true);
  assert.equal(toggle.hidden, true);

  // 2. Detecting media reveals #toggle automatically with updated count
  await normal.page.fetch('https://cdn.example/video.m3u8');
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(toggle.textContent, '下载 · 1');
  assert.equal(toggle.hidden, false);

  // 3. Disabling hideWhenEmpty keeps #toggle visible even with 0 media and persists preference
  const manual = createSandbox('https://news.example/article/2', { server: 'http://docker.local:6600', secret: 'sec' });
  const manualToggle = manual.root.getElementById('toggle');
  const manualHide = manual.root.getElementById('hide-empty');
  assert.equal(manualToggle.hidden, true);
  manualHide.checked = false;
  manualHide.onchange();
  assert.equal(manualToggle.hidden, false);
  assert.equal(manual.savedCalls.at(-1).hideWhenEmpty, false);

  const reloaded = createSandbox('https://news.example/article/3', manual.getStored());
  assert.equal(reloaded.root.getElementById('hide-empty').checked, false);
  assert.equal(reloaded.root.getElementById('toggle').hidden, false);

  // 4. Configured WebUI origin stays hidden even with media and hideWhenEmpty: false
  const webui = createSandbox('http://docker.local:6600/', { server: 'http://docker.local:6600', secret: 'sec', hideWhenEmpty: false });
  assert.equal(webui.root.getElementById('toggle').hidden, true);
  await webui.page.fetch('https://cdn.example/video.m3u8');
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(webui.root.getElementById('toggle').hidden, true);
});

test('ScriptCat extracts player tokenSession, updates URLs on token refresh, and attaches tokenSession only to HLS payloads', async () => {
  const inlineHtml = `
    <script>
      createPlayer({
        cloudType: 0,
        movieId: "movie-42",
        sessionId: "sess-xyz",
        guidId: "guid-77",
        apiSecret: "sec-key",
        apiSecretIssuedAt: 1795800000,
        timeDiff: 12000
      });
    </script>
  `;
  const session = extractTokenSessionFromText(inlineHtml, 'https://player.example/player/v2?id=42');
  assert.deepEqual(session, {
    origin: 'https://player.example',
    playerUrl: 'https://player.example/player/v2?id=42',
    sessionId: 'sess-xyz',
    movieId: 'movie-42',
    guidId: 'guid-77',
    apiSecret: 'sec-key',
    apiSecretIssuedAt: 1795800000,
    timeDiffMs: 12000,
  });
  assert.equal(extractTokenSessionFromText('<div>no player</div>', 'https://player.example/v'), null);

  const oldM3u8 = 'https://cdn.example/20261126/vid-1/1080p/video.m3u8?token=old&expires=100&token_path=%252Fvid-1%252F&vid=v1';
  const refreshedM3u8 = applyFreshTokenToUrl(oldM3u8, 'fresh-tok', 1999999999, '%252Fvid-1%252F', 'v2');
  const refreshedUrl = new URL(refreshedM3u8);
  assert.equal(refreshedUrl.searchParams.get('token'), 'fresh-tok');
  assert.equal(refreshedUrl.searchParams.get('expires'), '1999999999');
  assert.equal(refreshedUrl.searchParams.get('token_path'), '/vid-1/');
  assert.equal(refreshedUrl.searchParams.get('vid'), 'v2');
  assert.ok(!refreshedM3u8.includes('%252F'));

  const unrelated = 'https://cdn.example/other/playlist.m3u8';
  assert.equal(applyFreshTokenToUrl(unrelated, 'fresh-tok', 1999999999, '/vid-1/'), unrelated);

  const hlsPayload = payloadFor(
    { url: refreshedM3u8, type: 'application/vnd.apple.mpegurl', headers: {}, tokenSession: session },
    'Demo Title',
    'https://player.example/movie/detail/42.html',
    ''
  );
  assert.deepEqual(hlsPayload.tokenSession, session);

  const subPayload = payloadFor(
    { url: 'https://sub.example/zh.vtt', type: 'text/vtt', headers: {}, tokenSession: session },
    'Demo Title',
    'https://player.example/movie/detail/42.html',
    ''
  );
  assert.equal(subPayload.tokenSession, undefined);

  // Runtime: iframe script extraction + /api/token/generate live refresh updates resource & preserves selection
  class Element {
    constructor() {
      this.children = [];
      this.style = {};
      this.value = '';
      this.hidden = false;
      this.className = '';
    }
    append(...children) {
      this.children.push(...children);
    }
    replaceChildren() {
      this.children = [];
    }
  }
  const fields = new Map();
  const panel = new Element();
  panel.hidden = true;
  const root = {
    getElementById(id) {
      if (!fields.has(id)) fields.set(id, new Element());
      return fields.get(id);
    },
    querySelector: () => panel,
  };
  const posted = [];
  const page = {
    fetch: async url => {
      const u = String(url);
      if (u.endsWith('/api/token/generate')) {
        return new Response(
          JSON.stringify({
            data: { token: 'new-live-tok', expires: 1999999888, tokenPath: '%2Fvid-9%2F', vid: 'v9b' },
          }),
          { status: 200, headers: { 'Content-Type': 'application/json' } }
        );
      }
      return new Response('#EXTM3U\n#EXTINF:5,\nvideo0.ts\n#EXT-X-ENDLIST\n', {
        status: 200,
        headers: { 'Content-Type': 'application/vnd.apple.mpegurl' },
      });
    },
    XMLHttpRequest: class XHR {
      open() {}
      setRequestHeader() {}
      send() {}
    },
  };
  const window = {
    innerHeight: 844,
    addEventListener() {},
    top: { postMessage: msg => posted.push(msg) },
  };
  const iframeSandbox = {
    unsafeWindow: page,
    window,
    location: new URL('https://player.example/player/v2?payload=abc'),
    document: {
      title: 'Player',
      documentElement: new Element(),
      createElement: () => Object.assign(new Element(), { attachShadow: () => root }),
      referrerPolicy: '',
      addEventListener() {},
      querySelector: () => null,
      querySelectorAll: sel => (sel === 'script' ? [{ textContent: inlineHtml }] : []),
    },
    navigator: { userAgent: 'ua' },
    URL,
    Headers,
    TextDecoder,
    setInterval: () => 0,
    setTimeout,
    clearTimeout,
    GM_getValue: () => null,
    GM_setValue() {},
    GM_registerMenuCommand() {},
  };
  vm.runInNewContext(fs.readFileSync(require.resolve('../examples/scriptcat-downloader.user.js'), 'utf8'), iframeSandbox);
  await page.fetch('https://cdn.example/20261126/vid-9/1080p/video.m3u8?token=old-tok&expires=100&token_path=%2Fvid-9%2F&vid=v9');
  await page.fetch('https://player.example/api/token/generate', { method: 'POST', body: '{}' });
  await new Promise(resolve => setTimeout(resolve, 20));

  const tokenSessMsg = posted.find(m => m.kind === 'm3u8-dl-token-session-v1');
  assert.ok(tokenSessMsg);
  assert.equal(tokenSessMsg.tokenSession.movieId, 'movie-42');
  const itemMsg = posted.find(m => m.kind === 'm3u8-dl-resource-v1');
  assert.ok(itemMsg);
  assert.equal(itemMsg.item.tokenSession.sessionId, 'sess-xyz');
  const refreshMsg = posted.find(m => m.kind === 'm3u8-dl-token-refresh-v1');
  assert.ok(refreshMsg);
  assert.equal(refreshMsg.token, 'new-live-tok');

  // Dynamic createPlayer hook capture without any inline <script> tag + isolated player/media cookies
  const hookPosted = [];
  const hookSent = [];
  const hookFields = new Map();
  const hookPanel = new Element();
  hookPanel.hidden = true;
  const hookRoot = {
    getElementById(id) {
      if (!hookFields.has(id)) hookFields.set(id, new Element());
      return hookFields.get(id);
    },
    querySelector: () => hookPanel,
  };
  const hookPage = {
    fetch: async () =>
      new Response('#EXTM3U\n#EXTINF:5,\nvideo0.ts\n#EXT-X-ENDLIST\n', {
        status: 200,
        headers: { 'Content-Type': 'application/vnd.apple.mpegurl' },
      }),
    XMLHttpRequest: class XHR {
      open() {}
      setRequestHeader() {}
      send() {}
    },
  };
  const hookSandbox = {
    unsafeWindow: hookPage,
    window: {
      innerHeight: 844,
      addEventListener() {},
      top: { postMessage: msg => hookPosted.push(msg) },
    },
    location: new URL('https://player.example/player/v2?payload=dyn'),
    document: {
      title: 'Dynamic Player',
      documentElement: new Element(),
      createElement: () => Object.assign(new Element(), { attachShadow: () => hookRoot }),
      referrerPolicy: '',
      addEventListener() {},
      querySelector: () => null,
      querySelectorAll: () => [],
    },
    navigator: { userAgent: 'ua' },
    URL,
    Headers,
    TextDecoder,
    setInterval: () => 0,
    setTimeout,
    clearTimeout,
    GM_getValue: key => (key === 'docker-settings' ? { server: 'http://docker.local:6600', secret: 'sec' } : null),
    GM_setValue() {},
    GM_registerMenuCommand() {},
    GM_cookie(_action, details, cb) {
      if (details.url.includes('player.example')) {
        cb([{ domain: 'player.example', path: '/', name: 'cf_clearance', value: 'p1', secure: true }], null);
      } else {
        cb([{ domain: 'cdn.example', path: '/', name: 'cdn_sid', value: 'c1', secure: true }], null);
      }
    },
    GM_xmlhttpRequest(opts) {
      hookSent.push(opts);
      opts.onload({ status: 200, responseText: JSON.stringify({ code: 0, data: { added: 1, skipped: 0 } }) });
    },
  };
  hookSandbox.window.top = hookSandbox.window;
  vm.runInNewContext(fs.readFileSync(require.resolve('../examples/scriptcat-downloader.user.js'), 'utf8'), hookSandbox);
  let playerInitCalled = false;
  hookPage.createPlayer = opts => ({
    init() {
      playerInitCalled = Boolean(opts.movieId);
    },
  });
  hookPage
    .createPlayer({
      cloudType: 0,
      movieId: 'dyn-movie',
      sessionId: 'dyn-sess',
      guidId: 'dyn-guid',
      apiSecret: 'dyn-secret',
      apiSecretIssuedAt: 1795800999,
      timeDiff: 0,
    })
    .init();
  assert.equal(playerInitCalled, true);

  await hookPage.fetch('https://cdn.example/20261126/dyn-1/1080p/video.m3u8?token=t0&expires=1999999000&token_path=%2Fdyn-1%2F&vid=d1', {
    credentials: 'include',
  });
  await new Promise(resolve => setTimeout(resolve, 10));
  hookRoot.getElementById('toggle').onclick();
  hookRoot.getElementById('cookies').checked = true;
  hookRoot.getElementById('all').onclick();
  await hookRoot.getElementById('send').onclick();
  assert.equal(hookSent.length, 1);
  const sentPayload = JSON.parse(hookSent[0].data).data[0];
  assert.equal(sentPayload.tokenSession.movieId, 'dyn-movie');
  assert.equal(sentPayload.tokenSession.cookie, 'cf_clearance=p1');
  assert.equal(sentPayload.requestHeaders.cookie, 'cdn_sid=c1');
});
