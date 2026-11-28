<script setup lang="ts">
import type {
  ApiResponse,
  ExtractedMedia,
  ExtractionResult,
  RssActionResult,
  RssFeedItem,
  RssItemsPage,
  RssRefreshResult,
  RssSource,
  RssSourceMode,
  RssValidateResult,
  ScrapeConfig,
  ScrapeDataSource,
  ScrapeFolderTemplate,
  ScrapeItemOutcome,
  ScrapeJobSnapshot,
  ScrapeMode,
  ScrapePosterCrop,
  ScrapePreviewItem,
  ScrapeRunItemInput,
  ScrapeScanResult,
  ScrapeSourceType,
  ScrapeTransferMode,
  SegmentSnapshot,
  ServerConfig,
  TaskItem,
} from 'streamdock/contract';
import { computed, nextTick, onBeforeUnmount, onMounted, reactive, ref, watch, watchEffect } from 'vue';
import AppIcon from './AppIcon.vue';

type Skin = 'apple' | 'meridian';
type ThemeMode = 'system' | 'light' | 'dark';
type ViewTab = 'tasks' | 'rss' | 'scrape' | 'settings';
type SettingsTab = 'download' | 'network' | 'appearance';

const appVersion = typeof __APP_VERSION__ === 'string' && __APP_VERSION__ ? __APP_VERSION__ : 'dev';
const savedSkin = typeof localStorage !== 'undefined' ? localStorage.getItem('m3u8-skin') : null;
const skin = ref<Skin>(savedSkin === 'meridian' || savedSkin === 'apple' ? savedSkin : 'apple');
const savedTheme = typeof localStorage !== 'undefined' ? localStorage.getItem('m3u8-theme') : null;
const themeMode = ref<ThemeMode>(savedTheme === 'light' || savedTheme === 'dark' || savedTheme === 'system' ? savedTheme : 'system');
const mediaQuery = typeof window !== 'undefined' && window.matchMedia ? window.matchMedia('(prefers-color-scheme: dark)') : null;
const systemDark = ref(mediaQuery?.matches ?? false);
const resolvedTheme = computed(() => (themeMode.value === 'system' ? (systemDark.value ? 'dark' : 'light') : themeMode.value));

function onSystemThemeChange(event: MediaQueryListEvent) {
  systemDark.value = event.matches;
}

function setSkin(next: Skin) {
  skin.value = next;
  try {
    localStorage.setItem('m3u8-skin', next);
  } catch {
    /* Ignore storage errors. */
  }
}

function setThemeMode(next: ThemeMode) {
  themeMode.value = next;
  try {
    localStorage.setItem('m3u8-theme', next);
  } catch {
    /* Ignore storage errors. */
  }
}

function toggleTheme() {
  setThemeMode(resolvedTheme.value === 'dark' ? 'light' : 'dark');
}

watchEffect(() => {
  document.documentElement.dataset.skin = skin.value;
  document.documentElement.dataset.theme = resolvedTheme.value;
  const color =
    skin.value === 'meridian'
      ? resolvedTheme.value === 'dark'
        ? '#000000'
        : '#f4f4f6'
      : resolvedTheme.value === 'dark'
        ? '#17191e'
        : '#edf0f5';
  document.querySelector('meta[name="theme-color"]')?.setAttribute('content', color);
});

const view = ref<ViewTab>('tasks');
const settingsTab = ref<SettingsTab>('download');
const settingsBusy = ref(false);
const settingsError = ref('');

function createDefaultScrapeSources(): ScrapeDataSource[] {
  return [];
}

const scrapeScanDirs = ref<string[]>(['./downloads']);
const scrapeSources = ref<ScrapeDataSource[]>(createDefaultScrapeSources());
const scrapeImportFeedId = ref('');
const scrapeFeedPreset = ref('custom');
const scrapeRssUrl = ref('');
const scrapeReferer = ref('');
const scrapeMode = ref<ScrapeMode>('in-place');
const scrapeOutputDir = ref('');
const scrapeTransferMode = ref<ScrapeTransferMode>('move');
const scrapeFolderTemplate = ref<ScrapeFolderTemplate>('actor-title');
const scrapePosterCrop = ref<ScrapePosterCrop>('center');
const scrapeMinScore = ref(55);
const scrapeRecursive = ref(true);
const scrapeSkipExistingNfo = ref(true);
const scrapeDownloadImages = ref(true);
const scrapeOverwriteNfo = ref(false);
const scrapeConcurrency = ref(3);
const scrapeScanBusy = ref(false);
const scrapeStartBusy = ref(false);
const scrapeResumeBusy = ref(false);
const scrapeStopBusy = ref(false);
const scrapeJob = ref<ScrapeJobSnapshot | null>(null);
const scrapeConfigSavedAt = ref(0);
let scrapeJobRevision = 0;
let scrapeConfigRevision = 0;
let scrapeConfigTimer: ReturnType<typeof setTimeout> | undefined;
let hydratingScrapeConfig = false;
let savingScrapeConfig = false;
let scrapeConfigHydrated = false;
let lastSavedScrapeConfigJson = '';
const scrapeRunBusy = computed(() => scrapeStartBusy.value || scrapeResumeBusy.value || scrapeJob.value?.status === 'running');
const scrapePercent = computed(() =>
  scrapeJob.value && scrapeJob.value.total > 0 ? Math.min(100, Math.round((scrapeJob.value.done / scrapeJob.value.total) * 100)) : 0
);
const scrapeSpeedText = computed(() => {
  const job = scrapeJob.value;
  if (!job || job.done < 2) return '';
  const elapsedMs = job.elapsedMs && job.elapsedMs > 0 ? job.elapsedMs : job.startedAt ? (job.finishedAt || Date.now()) - job.startedAt : 0;
  const elapsedSec = elapsedMs / 1000;
  if (elapsedSec < 2) return '';
  const perMin = (job.done / elapsedSec) * 60;
  if (perMin >= 2) return `约 ${perMin.toFixed(1)} 个/分钟`;
  return `约 ${(elapsedSec / job.done).toFixed(1)} 秒/个`;
});
const scrapeError = ref('');
const scrapeScanResult = ref<ScrapeScanResult | null>(null);
const scrapeFilter = ref<'all' | 'matched' | 'low' | 'unmatched'>('all');
const scrapeSelected = reactive(new Set<string>());
const scrapeChoice = reactive<Record<string, string>>({});
let scrapeController: AbortController | undefined;

const tasks = ref<Record<string, TaskItem>>({});
const connection = ref<'connecting' | 'online' | 'offline'>('connecting');
const loading = ref(true);
const tab = ref<'active' | 'done' | 'error'>('active');
const search = ref('');
const busy = ref(false);
const pending = reactive(new Set<string>());
const selected = reactive(new Set<string>());
const batchBusy = ref(false);
const notice = ref('');
const noticeError = ref(false);
const formError = ref('');
const modalError = ref('');
const modalBusy = ref(false);
const urls = ref('');
const title = ref('');
const downloadType = ref<'auto' | 'm3u8' | 'file' | 'parser'>('auto');
const entryMode = ref<'download' | 'extract'>('download');
const entryTitle = computed(() => (entryMode.value === 'extract' ? '网页媒体提取' : '新建下载'));
const extractDepth = ref(1);
const extractPath = ref('');
const extractBusy = ref(false);
const extracted = ref<ExtractionResult | null>(null);
const extractedSelection = ref<string[]>([]);
const selectedExtracted = computed(() => extracted.value?.items.filter(item => extractedSelection.value.includes(item.url)) || []);
let extractionController: AbortController | undefined;
const source = ref('');
const headers = ref('');
const advanced = ref(false);
const newDownloadOpen = ref(false);

const detailsUrl = ref('');
const detailsTask = computed(() => tasks.value[detailsUrl.value]);
const segments = ref<SegmentSnapshot>({ fingerprint: '', items: [] });
const segmentLoading = ref(false);
const segmentError = ref('');
const segmentFilter = ref<'all' | 'done' | 'error' | 'pending'>('all');
const segmentPage = ref(1);
const selectedSegment = ref<number | null>(null);
let segmentTimer: ReturnType<typeof setInterval>;
const segmentCounts = computed(() => ({
  done: segments.value.items.filter(item => item.status === 'done').length,
  error: segments.value.items.filter(item => item.status === 'error').length,
  pending: segments.value.items.filter(item => item.status === 'pending' || item.status === 'downloading').length,
}));
const filteredSegments = computed(() =>
  segments.value.items.filter(item =>
    segmentFilter.value === 'all'
      ? true
      : segmentFilter.value === 'pending'
        ? ['pending', 'downloading'].includes(item.status)
        : item.status === segmentFilter.value
  )
);
const pageSegments = computed(() => filteredSegments.value.slice((segmentPage.value - 1) * 100, segmentPage.value * 100));
const segmentPages = computed(() => Math.max(1, Math.ceil(filteredSegments.value.length / 100)));
const inspectedSegment = computed(() => segments.value.items.find(item => item.index === selectedSegment.value));
const segmentLabels = { pending: '未完成', downloading: '下载中', done: '已完成', error: '下载失败' };
watch(
  () => detailsTask.value?.status,
  (status, previous) => {
    if (detailsUrl.value && previous && status !== previous && detailsTask.value && !isFile(detailsTask.value) && !detailsTask.value.browserCapture) {
      void loadSegments();
    }
  }
);

function readSavedToken() {
  try {
    return localStorage.getItem('m3u8-token') || sessionStorage.getItem('m3u8-token') || localStorage.getItem('token') || '';
  } catch {
    return '';
  }
}

function saveToken(value: string, persist: boolean) {
  try {
    if (persist) {
      localStorage.setItem('m3u8-token', value);
      sessionStorage.removeItem('m3u8-token');
    } else {
      sessionStorage.setItem('m3u8-token', value);
      localStorage.removeItem('m3u8-token');
    }
    localStorage.removeItem('token');
  } catch {
    /* Ignore storage errors. */
  }
}

function clearSavedToken() {
  try {
    localStorage.removeItem('m3u8-token');
    localStorage.removeItem('token');
    sessionStorage.removeItem('m3u8-token');
  } catch {
    /* Ignore storage errors. */
  }
}

const loginOpen = ref(false);
const editTask = ref<TaskItem | null>(null);
const editHeaders = ref('');
const removeTasks = ref<TaskItem[]>([]);
const removeFile = ref(false);
const removeCache = ref(true);
const password = ref('');
const rememberLogin = ref(true);
const token = ref(readSavedToken());
const inputRef = ref<HTMLTextAreaElement>();
const modalRef = ref<HTMLDialogElement>();
const config = reactive<ServerConfig>({
  saveDir: './downloads',
  threadNum: 4,
  maxDownloads: 2,
  delCache: true,
  ffmpegPath: '',
  headers: '',
  mergeTimeout: 180,
  proxyMode: 'system',
  proxyUrl: '',
  noProxy: '',
});

const rssSources = ref<RssSource[]>([]);
const rssUnreadTotal = computed(() => rssSources.value.reduce((sum, item) => sum + (item.unreadCount || 0), 0));
const rssSelectedId = ref('');
const rssDetailOpen = ref(false);
const activeRssSource = computed(() => rssSources.value.find(item => item.id === rssSelectedId.value) || rssSources.value[0]);
const rssItemsPage = ref<RssItemsPage | null>(null);
const rssItemsLoading = ref(false);
const rssItemsError = ref('');
const rssStatusFilter = ref<'all' | 'new' | 'downloaded' | 'ignored' | 'filtered' | 'seed-skipped'>('all');
const rssSearch = ref('');
const rssSort = ref<'newest' | 'oldest'>('newest');
const rssPage = ref(1);
const rssPageSize = ref(50);
const rssSelectedGuids = reactive(new Set<string>());
const rssRefreshingId = ref('');
const rssActionBusy = ref(false);
let rssSearchTimer: ReturnType<typeof setTimeout>;
let rssItemsSeq = 0;

const rssEditorOpen = ref(false);
const rssEditingSource = ref<RssSource | null>(null);
const rssEditorTab = ref<'basic' | 'filter' | 'advanced'>('basic');
const rssDeleteSource = ref<RssSource | null>(null);
const rssValidateBusy = ref(false);
const rssValidateResult = ref<RssValidateResult | null>(null);
const rssValidateError = ref('');
const rssForm = reactive({
  mode: 'rss' as RssSourceMode,
  url: '',
  name: '',
  enabled: true,
  autoDownload: false,
  intervalMinutes: 30,
  maxPerFetch: 20,
  itemPattern: '',
  includePattern: '',
  excludePattern: '',
  useRegex: false,
  smartEpisode: false,
  sizeMinText: '',
  sizeMaxText: '',
  saveDir: '',
  referer: '',
  cookies: '',
  userAgent: '',
  autoScrape: true,
  scrapeMode: 'in-place' as ScrapeMode,
  scrapeOutputDir: '',
  scrapeTransferMode: 'move' as ScrapeTransferMode,
  scrapeFolderTemplate: 'actor-title' as ScrapeFolderTemplate,
  scrapePosterCrop: 'center' as ScrapePosterCrop,
  scrapeDownloadImages: true,
});

function formatBytesInput(value = 0) {
  if (!value || value <= 0) return '';
  if (value >= 1000 ** 3 && value % 1000 ** 3 === 0) return `${value / 1000 ** 3} GB`;
  if (value >= 1000 ** 2) return `${Number((value / 1000 ** 2).toFixed(1))} MB`;
  if (value >= 1000) return `${Number((value / 1000).toFixed(1))} KB`;
  return String(value);
}

function parseHumanBytes(raw: string): number | null {
  const text = String(raw || '').trim();
  if (!text) return 0;
  const m = /^(\d+(?:\.\d+)?)\s*(B|K|KB|KiB|M|MB|MiB|G|GB|GiB|T|TB|TiB)?$/i.exec(text);
  if (!m) return null;
  const val = Number.parseFloat(m[1]);
  if (!Number.isFinite(val) || val < 0) return null;
  const unit = (m[2] || 'B').toUpperCase();
  const power = unit.startsWith('K') ? 1 : unit.startsWith('M') ? 2 : unit.startsWith('G') ? 3 : unit.startsWith('T') ? 4 : 0;
  return Math.trunc(val * (unit.endsWith('IB') || power === 0 ? 1024 : 1000) ** power);
}

function matchPreviewExpr(titleText: string, expr: string, useRegex: boolean, defaultWhenEmpty: boolean) {
  const trimmed = expr.trim();
  if (!trimmed) return defaultWhenEmpty;
  if (useRegex) {
    try {
      return new RegExp(trimmed, 'i').test(titleText);
    } catch {
      return true;
    }
  }
  const alts = trimmed
    .split('|')
    .map(alt => alt.trim().toLowerCase().split(/\s+/).filter(Boolean))
    .filter(words => words.length > 0);
  if (!alts.length) return true;
  const lower = titleText.toLowerCase();
  return alts.some(words => words.every(word => lower.includes(word)));
}

const rssFilterPreview = computed(() => {
  const minBytes = parseHumanBytes(rssForm.sizeMinText);
  const maxBytes = parseHumanBytes(rssForm.sizeMaxText);
  const sizeError =
    minBytes === null || maxBytes === null
      ? '体积格式示例：200 MB、1.5 GB（留空表示不限）'
      : minBytes > 0 && maxBytes > 0 && minBytes > maxBytes
        ? '体积下限不能大于体积上限'
        : '';
  const sampleSource = rssValidateResult.value?.sampleItems || (rssEditingSource.value ? rssItemsPage.value?.items || [] : []);
  const seenEpisodes = new Set<string>();
  let accepted = 0;
  const evaluated = sampleSource.map(item => {
    let pass = true;
    let label = '通过';
    if (!matchPreviewExpr(item.title, rssForm.includePattern, rssForm.useRegex, true)) {
      pass = false;
      label = '未命中包含';
    } else if (rssForm.excludePattern.trim() && matchPreviewExpr(item.title, rssForm.excludePattern, rssForm.useRegex, false)) {
      pass = false;
      label = '命中排除';
    } else if (!sizeError && item.enclosureLength > 0) {
      if ((minBytes || 0) > 0 && item.enclosureLength < (minBytes || 0)) {
        pass = false;
        label = '小于下限';
      } else if ((maxBytes || 0) > 0 && item.enclosureLength > (maxBytes || 0)) {
        pass = false;
        label = '超过上限';
      }
    }
    if (pass && rssForm.smartEpisode && item.episodeKey) {
      if (seenEpisodes.has(item.episodeKey)) {
        pass = false;
        label = `重复 ${item.episodeKey}`;
      } else {
        seenEpisodes.add(item.episodeKey);
      }
    }
    if (pass) accepted++;
    return { guid: item.guid, title: item.title, pass, label };
  });
  return {
    sizeError,
    total: sampleSource.length,
    accepted,
    rejected: sampleSource.length - accepted,
    rows: evaluated.slice(0, 6),
  };
});

let socket: WebSocket | undefined;
let reconnectTimer: ReturnType<typeof setTimeout>;
let noticeTimer: ReturnType<typeof setTimeout>;
let stopped = false;
let tasksRevision = 0;
let loadSequence = 0;
const all = computed(() => Object.values(tasks.value));
const active = computed(() => all.value.filter(task => task.status !== 'done'));
const complete = computed(() => all.value.filter(task => task.status === 'done'));
const completedBytes = computed(() => complete.value.reduce((sum, task) => sum + (task.downloadedSize || task.size || 0), 0));
const failedSegmentTasks = computed(() => active.value.filter(task => task.status === 'error' && task.tsFailed && task.hasSegmentDetails));
const errors = computed(() => active.value.filter(task => task.status === 'error').length);
const running = computed(() => active.value.filter(task => task.status === 'resume').length);
const runningRatio = computed(() => Math.min(100, Math.round((running.value / Math.max(1, config.maxDownloads || 1)) * 100)));
const speed = computed(() => active.value.reduce((sum, task) => sum + (task.status === 'resume' ? task.speed || 0 : 0), 0));
const visible = computed(() =>
  (tab.value === 'done' ? complete.value : tab.value === 'error' ? active.value.filter(task => task.status === 'error') : active.value)
    .filter(task => `${name(task)} ${task.browserSource || task.url}`.toLowerCase().includes(search.value.toLowerCase()))
    .sort((a, b) => (b.startTime || 0) - (a.startTime || 0))
);
const selectedTasks = computed(() => visible.value.filter(task => selected.has(task.url)));
const allSelected = computed(() => visible.value.length > 0 && selectedTasks.value.length === visible.value.length);
watch(visible, items => {
  const urlSet = new Set(items.map(task => task.url));
  for (const url of selected) if (!urlSet.has(url)) selected.delete(url);
});
const modalOpen = computed(
  () => newDownloadOpen.value || !!detailsUrl.value || loginOpen.value || !!editTask.value || removeTasks.value.length > 0 || rssEditorOpen.value || !!rssDeleteSource.value
);
watch(modalOpen, open => {
  document.documentElement.classList.toggle('modal-open', open);
});
const lines = computed(() =>
  urls.value
    .split(/\r?\n/)
    .map(line => line.trim())
    .filter(Boolean)
);
const originHint = computed(() => {
  try {
    return `${new URL(lines.value[0]).origin}/`;
  } catch {
    return '自动使用资源域名';
  }
});

function bytes(value = 0) {
  if (!Number.isFinite(value) || value <= 0) return '0 B';
  const index = Math.min(Math.floor(Math.log(value) / Math.log(1024)), 3);
  return `${(value / 1024 ** index).toFixed(index ? 1 : 0)} ${['B', 'KB', 'MB', 'GB'][index]}`;
}
function host(url: string) {
  try {
    return new URL(url).hostname;
  } catch {
    return url;
  }
}
function taskSource(task: TaskItem) {
  return task.url.startsWith('browser:') ? host(task.browserSource || '浏览器窗口') : host(task.url);
}
function isFile(task: TaskItem) {
  return task.downloadType ? task.downloadType === 'file' : task.options?.type === 'file';
}
function kind(task: TaskItem) {
  return isFile(task) ? /\.([a-z0-9]{1,5})$/i.exec(task.filename || '')?.[1].toUpperCase() || 'FILE' : 'HLS';
}
function formatTone(task: TaskItem) {
  if (!isFile(task)) return 'blue';
  const ext = kind(task);
  if (['VTT', 'SRT', 'ASS', 'SSA'].includes(ext)) return 'teal';
  if (['MP3', 'M4A', 'AAC', 'WAV', 'FLAC', 'OGG', 'OPUS', 'WEBA'].includes(ext)) return 'green';
  return 'purple';
}
function formatTime(ts?: number) {
  if (!ts) return '';
  return new Date(ts).toLocaleString('zh-CN', { month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit', hour12: false });
}
function detailLabel(task: TaskItem) {
  return `查看 ${name(task)} 的${isFile(task) ? '下载' : '分片'}详情`;
}
function name(task: TaskItem) {
  const filename = task.filename || task.options?.filename || '';
  return /^[a-f\d]{32}\.mp4$/.test(filename) || !filename ? `视频 · ${host(task.url)}` : filename;
}
function label(task: TaskItem) {
  if (task.status === 'done') return '已完成';
  if (task.status === 'pause') return '已暂停';
  if (task.status === 'error') return '需要重试';
  if (task.status === 'pending') return '排队中';
  if (task.phase === 'merging') return '正在合并';
  if (task.browserCapture) return task.phase === 'parsing' ? '等待浏览器播放' : '正在保存分片';
  return task.phase === 'parsing' ? '解析链接' : '下载中';
}
function progress(task: TaskItem) {
  return task.status === 'done' ? 100 : task.phase === 'merging' ? task.mergeProgress || 0 : Math.min(99, task.progress || 0);
}
function notify(message: string, error = false) {
  notice.value = message;
  noticeError.value = error;
  clearTimeout(noticeTimer);
  noticeTimer = setTimeout(() => {
    notice.value = '';
  }, 5000);
}

async function api<T = unknown>(path: string, body?: unknown, signal?: AbortSignal): Promise<T> {
  const response = await fetch(`/api/${path}`, {
    method: body === undefined ? 'GET' : 'POST',
    headers: { 'content-type': 'application/json', authorization: token.value },
    body: body === undefined ? undefined : JSON.stringify(body),
    signal,
  });
  const result = (await response.json()) as ApiResponse<T>;
  if (response.status === 401) {
    if (!loginOpen.value) {
      clearSavedToken();
      token.value = '';
      loginOpen.value = true;
      await openModal();
    }
  }
  if (!response.ok || result.code) throw new Error(result.message || '请求失败，请重试');
  return result.data as T;
}

async function load() {
  const sequence = ++loadSequence;
  const revision = tasksRevision;
  const jobRevision = scrapeJobRevision;
  const cfgRevision = scrapeConfigRevision;
  try {
    const [items, settingsData, sourcesData, jobData, scrapeCfgData] = await Promise.all([
      api<Record<string, TaskItem>>('tasks'),
      api<ServerConfig>('config'),
      api<RssSource[]>('rss/sources'),
      api<ScrapeJobSnapshot | null>('scrape/job'),
      api<ScrapeConfig>('scrape/config'),
    ]);
    if (sequence !== loadSequence) return;
    if (revision === tasksRevision) tasks.value = items;
    if (jobRevision === scrapeJobRevision) applyScrapeJob(jobData ?? null);
    Object.assign(config, settingsData);
    rssSources.value = Array.isArray(sourcesData) ? sourcesData : [];
    if (rssSelectedId.value && !rssSources.value.some(item => item.id === rssSelectedId.value)) {
      rssSelectedId.value = '';
      rssDetailOpen.value = false;
      rssItemsPage.value = null;
    }
    if (!rssSelectedId.value && rssSources.value.length) rssSelectedId.value = rssSources.value[0].id;
    if (view.value === 'rss' && activeRssSource.value && !rssItemsPage.value && !rssItemsLoading.value) void loadRssItems();
    if (scrapeCfgData && (!scrapeConfigHydrated || (cfgRevision === scrapeConfigRevision && !scrapeConfigTimer))) {
      applyScrapeConfig(scrapeCfgData);
    }
    loginOpen.value = false;
  } catch (error) {
    if (!loginOpen.value) notify((error as Error).message, true);
  } finally {
    if (sequence === loadSequence) loading.value = false;
  }
}

function connect() {
  if (stopped) return;
  clearTimeout(reconnectTimer);
  socket?.close();
  connection.value = 'connecting';
  const ws = new WebSocket(`${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}/ws?token=${encodeURIComponent(token.value)}`);
  socket = ws;
  ws.onmessage = event => {
    if (socket !== ws) return;
    try {
      const message = JSON.parse(event.data);
      if (message.type === 'serverInfo') {
        connection.value = 'online';
      }
      if (message.type === 'tasks') {
        tasksRevision++;
        tasks.value = message.data;
      }
      if (message.type === 'rssSources') {
        rssSources.value = Array.isArray(message.data) ? message.data : [];
        if (rssSelectedId.value && !rssSources.value.some(item => item.id === rssSelectedId.value)) {
          rssSelectedId.value = '';
          rssDetailOpen.value = false;
          rssItemsPage.value = null;
        }
        if (!rssSelectedId.value && rssSources.value.length) rssSelectedId.value = rssSources.value[0].id;
        if (view.value === 'rss' && activeRssSource.value && !rssItemsLoading.value) void loadRssItems();
      }
      if (message.type === 'scrapeConfig' && message.data) {
        scrapeConfigRevision++;
        const hasDraftSource = scrapeSources.value.some(src => !String(src.url || '').trim());
        if (!scrapeConfigTimer && !hydratingScrapeConfig && !savingScrapeConfig && !hasDraftSource) {
          applyScrapeConfig(message.data as ScrapeConfig);
        }
      }
      if (message.type === 'scrapeJob') {
        applyScrapeJob((message.data as ScrapeJobSnapshot | null | undefined) ?? null, true);
      }
    } catch {
      notify('任务数据接收失败，正在重新同步', true);
      void load();
    }
  };
  ws.onclose = event => {
    if (stopped || socket !== ws) return;
    connection.value = 'offline';
    if (event.code === 1008) {
      if (!loginOpen.value) {
        clearSavedToken();
        token.value = '';
        loginOpen.value = true;
        void openModal();
      }
      return;
    }
    reconnectTimer = setTimeout(() => {
      connect();
      void load();
    }, 3000);
  };
  ws.onerror = () => {
    connection.value = 'offline';
  };
}

function combinedHeaders() {
  const text = headers.value.trim();
  const parsed: Record<string, string> = {};
  if (text.startsWith('{')) Object.assign(parsed, JSON.parse(text));
  else
    for (const line of text.split(/\r?\n/)) {
      if (!line.trim()) continue;
      const index = line.indexOf(':');
      if (index < 1) throw new Error('请求头格式不正确，请每行填写「名称: 值」');
      parsed[line.slice(0, index).trim().toLowerCase()] = line.slice(index + 1).trim();
    }
  if (source.value.trim()) {
    const url = new URL(source.value.trim());
    if (!['http:', 'https:'].includes(url.protocol)) throw new Error('来源页应为 http 或 https 地址');
    for (const key of Object.keys(parsed)) if (key.toLowerCase() === 'referer') delete parsed[key];
    parsed.referer = url.href;
  }
  return JSON.stringify(parsed);
}

async function submit() {
  if (!lines.value.length) {
    inputRef.value?.focus();
    return;
  }
  busy.value = true;
  formError.value = '';
  try {
    if (lines.value.length > 100) throw new Error('一次最多添加 100 个链接');
    const custom = combinedHeaders();
    await api('download', {
      list: lines.value.map((url, index) => ({
        url,
        type: downloadType.value,
        headers: custom,
        ...(title.value.trim() ? { filename: `${title.value.trim()}${lines.value.length > 1 ? `-${index + 1}` : ''}` } : {}),
      })),
    });
    urls.value = '';
    title.value = '';
    tab.value = 'active';
    view.value = 'tasks';
    notify('已加入下载队列');
    await load();
  } catch (error) {
    formError.value = (error as Error).message;
  } finally {
    busy.value = false;
  }
}

function cancelExtraction() {
  extractionController?.abort();
  extractionController = undefined;
  extractBusy.value = false;
}
function switchEntry(mode: 'download' | 'extract') {
  cancelExtraction();
  entryMode.value = mode;
  formError.value = '';
  modalError.value = '';
}
watch([urls, extractDepth, extractPath], () => {
  cancelExtraction();
  extracted.value = null;
  extractedSelection.value = [];
});

async function runExtraction() {
  if (extractBusy.value) {
    cancelExtraction();
    return;
  }
  if (!lines.value.length) {
    inputRef.value?.focus();
    return;
  }
  const controller = new AbortController();
  extractionController = controller;
  extractBusy.value = true;
  formError.value = '';
  modalError.value = '';
  extracted.value = null;
  extractedSelection.value = [];
  try {
    const result = await api<ExtractionResult>(
      'extract',
      { urls: lines.value, deep: extractDepth.value, pathFilter: extractPath.value.trim(), headers: combinedHeaders() },
      controller.signal
    );
    if (controller.signal.aborted) return;
    extracted.value = result;
    extractedSelection.value = result.items.map(item => item.url);
  } catch (error) {
    if (!controller.signal.aborted) formError.value = (error as Error).message;
  } finally {
    if (extractionController === controller) {
      extractionController = undefined;
      extractBusy.value = false;
    }
  }
}

function toggleExtractedAll() {
  extractedSelection.value = selectedExtracted.value.length === extracted.value?.items.length ? [] : extracted.value?.items.map(item => item.url) || [];
}
function extractedName(item: ExtractedMedia) {
  if (!item.title) return undefined;
  let extension = '.mp4';
  if (item.type === 'file') {
    try {
      extension = /\.([a-z0-9]{1,8})$/i.exec(new URL(item.url).pathname)?.[0] || '.mp4';
    } catch {
      /* Keep a usable media extension. */
    }
  }
  return `${item.title.slice(0, 120).replace(/\.(mp4|m3u8|vtt|srt)$/i, '')}${extension}`;
}
async function submitExtracted() {
  formError.value = '';
  try {
    const chosen = [...selectedExtracted.value];
    if (!chosen.length) throw new Error('请先提取并勾选要下载的媒体');
    const custom = JSON.parse(combinedHeaders());
    await api('download', {
      list: chosen.map(item => ({
        url: item.url,
        filename: extractedName(item),
        type: item.type,
        sourceUrl: item.source,
        headerSourceUrl: item.credentialSource || item.source,
        headers: custom,
      })),
    });
    notify(`已添加 ${chosen.length} 个下载`);
    tab.value = 'active';
    view.value = 'tasks';
    await load();
  } catch (error) {
    formError.value = (error as Error).message;
  }
}

async function action(task: TaskItem, type: 'pause' | 'resume' | 'retry-failed') {
  pending.add(task.url);
  try {
    await api(type, { urls: [task.url] });
    await load();
  } catch (error) {
    notify((error as Error).message, true);
  } finally {
    pending.delete(task.url);
  }
}

function toggleAll() {
  if (allSelected.value) selected.clear();
  else for (const task of visible.value) selected.add(task.url);
}
function toggleTask(url: string) {
  if (selected.has(url)) selected.delete(url);
  else selected.add(url);
}

function navigateTabs(event: KeyboardEvent) {
  const tabs = ['active', 'done', 'error'] as const;
  const index = tabs.indexOf(tab.value);
  const next =
    event.key === 'ArrowRight'
      ? (index + 1) % tabs.length
      : event.key === 'ArrowLeft'
        ? (index + tabs.length - 1) % tabs.length
        : event.key === 'Home'
          ? 0
          : event.key === 'End'
            ? tabs.length - 1
            : -1;
  if (next < 0) return;
  event.preventDefault();
  tab.value = tabs[next];
  document.getElementById(`${tabs[next]}-tab`)?.focus();
}

function navigateSettingsTabs(event: KeyboardEvent) {
  const tabs = ['download', 'network', 'appearance'] as const;
  const index = tabs.indexOf(settingsTab.value);
  const next =
    event.key === 'ArrowRight'
      ? (index + 1) % tabs.length
      : event.key === 'ArrowLeft'
        ? (index + tabs.length - 1) % tabs.length
        : event.key === 'Home'
          ? 0
          : event.key === 'End'
            ? tabs.length - 1
            : -1;
  if (next < 0) return;
  event.preventDefault();
  settingsTab.value = tabs[next];
  document.getElementById(`settings-${tabs[next]}-tab`)?.focus();
}

function batchTargets(type: 'pause' | 'resume' | 'retry-failed', selectedOnly = false) {
  return (selectedOnly ? selectedTasks.value : all.value).filter(
    task =>
      !pending.has(task.url) &&
      (type === 'pause'
        ? ['resume', 'pending'].includes(task.status)
        : type === 'resume'
          ? ['pause', 'error'].includes(task.status)
          : task.status === 'error' && task.tsFailed && task.hasSegmentDetails)
  );
}
async function batch(type: 'pause' | 'resume' | 'retry-failed', selectedOnly = false) {
  if (batchBusy.value) return;
  const targets = batchTargets(type, selectedOnly).map(task => task.url);
  if (!targets.length) return;
  batchBusy.value = true;
  for (const url of targets) pending.add(url);
  try {
    await api(type, { urls: targets });
    notify(type === 'pause' ? `已暂停 ${targets.length} 个任务，缓存已保留` : `已提交 ${targets.length} 个任务${type === 'retry-failed' ? '的失败分片重试' : '继续下载'}`);
  } catch (error) {
    notify((error as Error).message, true);
  } finally {
    for (const url of targets) pending.delete(url);
    batchBusy.value = false;
    await load();
  }
}

async function openModal() {
  await nextTick();
  if (!modalRef.value?.open) modalRef.value?.showModal();
}
function openSettings(nextTab: SettingsTab = 'download') {
  view.value = 'settings';
  settingsTab.value = nextTab;
  settingsError.value = '';
}
async function saveSettings() {
  if (settingsBusy.value) return;
  settingsBusy.value = true;
  settingsError.value = '';
  try {
    await api('config', { ...config });
    notify('下载设置已保存');
  } catch (error) {
    settingsError.value = (error as Error).message;
  } finally {
    settingsBusy.value = false;
  }
}
function closeModal() {
  if (loginOpen.value || modalBusy.value) return;
  cancelExtraction();
  clearInterval(segmentTimer);
  detailsUrl.value = '';
  newDownloadOpen.value = false;
  editTask.value = null;
  removeTasks.value = [];
  rssEditorOpen.value = false;
  rssEditingSource.value = null;
  rssDeleteSource.value = null;
  rssValidateResult.value = null;
  rssValidateError.value = '';
  modalError.value = '';
  modalRef.value?.close();
}
async function edit(task: TaskItem) {
  editTask.value = task;
  modalError.value = '';
  editHeaders.value = typeof task.options?.headers === 'string' ? task.options.headers : JSON.stringify(task.options?.headers || {}, null, 2);
  await openModal();
}
async function remove(items: TaskItem[]) {
  if (batchBusy.value || !items.length) return;
  removeTasks.value = [...items];
  removeFile.value = false;
  removeCache.value = true;
  modalError.value = '';
  await openModal();
}
async function modalSubmit() {
  modalBusy.value = true;
  modalError.value = '';
  try {
    if (loginOpen.value) {
      const previous = token.value;
      const candidate = password.value.trim();
      if (!candidate) throw new Error('请输入访问密码');
      token.value = candidate;
      try {
        await api('tasks');
      } catch (error) {
        token.value = previous;
        throw error;
      }
      saveToken(candidate, rememberLogin.value);
      password.value = '';
      loginOpen.value = false;
      connect();
      await load();
    } else if (newDownloadOpen.value) {
      if (entryMode.value === 'extract') await submitExtracted();
      else await submit();
      if (formError.value) throw new Error(formError.value);
    } else if (editTask.value) {
      await api('download', { list: [{ url: editTask.value.url, ...editTask.value.options, headers: editHeaders.value }] });
      notify('请求头已更新，任务已继续');
      await load();
    } else if (removeTasks.value.length) {
      await api('delete', { urls: removeTasks.value.map(task => task.url), deleteCache: removeCache.value, deleteVideo: removeFile.value });
      notify(`已移除 ${removeTasks.value.length} 个任务`);
      await load();
    } else if (rssEditorOpen.value) {
      const sizeMinBytes = parseHumanBytes(rssForm.sizeMinText);
      const sizeMaxBytes = parseHumanBytes(rssForm.sizeMaxText);
      if (sizeMinBytes === null || sizeMaxBytes === null) throw new Error('体积格式无效，示例：200 MB、1.5 GB');
      if (sizeMinBytes > 0 && sizeMaxBytes > 0 && sizeMinBytes > sizeMaxBytes) throw new Error('体积下限不能大于体积上限');
      if (rssForm.autoScrape && rssForm.scrapeMode === 'organize' && !rssForm.scrapeOutputDir.trim()) {
        throw new Error('启用归档整理模式时请填写刮削输出目录');
      }
      const payload = {
        mode: rssForm.mode,
        url: rssForm.url.trim(),
        name: rssForm.name.trim(),
        enabled: rssForm.enabled,
        autoDownload: rssForm.autoDownload,
        intervalMinutes: rssForm.intervalMinutes,
        maxPerFetch: rssForm.maxPerFetch,
        itemPattern: rssForm.itemPattern.trim(),
        includePattern: rssForm.includePattern.trim(),
        excludePattern: rssForm.excludePattern.trim(),
        useRegex: rssForm.useRegex,
        smartEpisode: rssForm.smartEpisode,
        sizeMinBytes,
        sizeMaxBytes,
        saveDir: rssForm.saveDir.trim(),
        referer: rssForm.referer.trim(),
        cookies: rssForm.cookies.trim(),
        userAgent: rssForm.userAgent.trim(),
        autoScrape: rssForm.autoScrape,
        scrapeMode: rssForm.scrapeMode,
        scrapeOutputDir: rssForm.scrapeOutputDir.trim(),
        scrapeTransferMode: rssForm.scrapeTransferMode,
        scrapeFolderTemplate: rssForm.scrapeFolderTemplate,
        scrapePosterCrop: rssForm.scrapePosterCrop,
        scrapeDownloadImages: rssForm.scrapeDownloadImages,
      };
      if (rssEditingSource.value) {
        await api<RssSource>(`rss/sources/${encodeURIComponent(rssEditingSource.value.id)}`, payload);
        notify('订阅源已更新');
        await load();
        if (view.value === 'rss' && activeRssSource.value) await loadRssItems();
      } else {
        const created = await api<RssSource>('rss/sources', { ...payload, fetchNow: true });
        rssSelectedId.value = created.id;
        rssPage.value = 1;
        rssDetailOpen.value = true;
        notify('已添加订阅源，正在后台拉取全量条目');
        await load();
        if (view.value === 'rss') await loadRssItems();
      }
    } else if (rssDeleteSource.value) {
      const targetId = rssDeleteSource.value.id;
      await api(`rss/sources/${encodeURIComponent(targetId)}/delete`, {});
      notify(`已删除订阅源「${rssDeleteSource.value.name}」`);
      if (rssSelectedId.value === targetId) {
        rssSelectedId.value = '';
        rssItemsPage.value = null;
        rssDetailOpen.value = false;
      }
      await load();
      if (view.value === 'rss' && activeRssSource.value) await loadRssItems();
    }
    modalBusy.value = false;
    closeModal();
  } catch (error) {
    modalError.value = (error as Error).message;
    await nextTick();
    modalRef.value?.querySelector('[role="alert"]')?.scrollIntoView({ block: 'nearest' });
  } finally {
    modalBusy.value = false;
  }
}

function eta(seconds = 0) {
  if (!seconds || !Number.isFinite(seconds)) return '计算中';
  if (seconds < 60) return `${seconds} 秒`;
  return `${Math.floor(seconds / 60)} 分 ${seconds % 60} 秒`;
}
async function newDownload(mode: 'download' | 'extract' = 'download') {
  switchEntry(mode);
  newDownloadOpen.value = true;
  await openModal();
  inputRef.value?.focus();
}
async function loadSegments() {
  const current = detailsUrl.value;
  if (!current) return;
  try {
    const response = await api<SegmentSnapshot>(`segments?url=${encodeURIComponent(current)}`);
    if (detailsUrl.value === current) {
      segments.value = response;
      segmentPage.value = Math.min(segmentPage.value, segmentPages.value);
      segmentError.value = '';
    }
  } catch (error) {
    if (detailsUrl.value === current) segmentError.value = (error as Error).message;
  } finally {
    segmentLoading.value = false;
  }
}
async function details(task: TaskItem) {
  clearInterval(segmentTimer);
  detailsUrl.value = task.url;
  segmentLoading.value = !isFile(task) && !task.browserCapture;
  segments.value = { fingerprint: '', items: [] };
  selectedSegment.value = null;
  segmentFilter.value = 'all';
  segmentPage.value = 1;
  await openModal();
  if (isFile(task) || task.browserCapture) return;
  await loadSegments();
  if (detailsUrl.value) segmentTimer = setInterval(() => { void loadSegments(); }, 1000);
}
function filterSegments(filter: 'all' | 'done' | 'error' | 'pending') {
  segmentFilter.value = filter;
  segmentPage.value = 1;
}
const visibleScrapeItems = computed(() => {
  const items = scrapeScanResult.value?.items || [];
  if (scrapeFilter.value === 'matched') return items.filter(item => item.status === 'matched');
  if (scrapeFilter.value === 'low') return items.filter(item => item.status === 'low-confidence');
  if (scrapeFilter.value === 'unmatched') return items.filter(item => item.status === 'unmatched' || item.status === 'skipped-nfo');
  return items;
});
const selectableScrapeItems = computed(() => visibleScrapeItems.value.filter(item => !!scrapeChoice[item.filePath]));
const allScrapeSelected = computed(
  () => selectableScrapeItems.value.length > 0 && selectableScrapeItems.value.every(item => scrapeSelected.has(item.filePath))
);
function toggleAllScrape() {
  if (allScrapeSelected.value) {
    for (const item of selectableScrapeItems.value) scrapeSelected.delete(item.filePath);
  } else {
    for (const item of selectableScrapeItems.value) scrapeSelected.add(item.filePath);
  }
}
function toggleScrapeItem(filePath: string) {
  if (scrapeSelected.has(filePath)) scrapeSelected.delete(filePath);
  else scrapeSelected.add(filePath);
}
function onScrapeChoiceChange(item: ScrapePreviewItem) {
  if (scrapeChoice[item.filePath]) scrapeSelected.add(item.filePath);
  else scrapeSelected.delete(item.filePath);
}
const visibleScrapeOutcomes = computed(() => scrapeJob.value?.results || []);
const scrapeTimingSummary = computed(() => {
  const stats = scrapeJob.value?.timing;
  if (!stats?.count) return null;
  const { count, avgTotalMs: avgTotal, avgDetailMs: avgDetail, avgTransferMs: avgTransfer, avgArtworkMs: avgArt, avgCropMs: avgCrop, crossFsCount } = stats;
  let hint = '各阶段耗时均处于正常范围，如仍有延迟可适当调高刮削并发数。';
  let warn = false;
  if (crossFsCount > 0) {
    hint = `发现 ${crossFsCount} 个跨卷复制导致整文件拷贝，建议改用「原地刮削」或同卷输出目录，速度可提升数十倍。`;
    warn = true;
  } else if (avgDetail >= 1500 && avgDetail >= avgArt && avgDetail >= avgCrop) {
    hint = `主要耗时在请求详情页（平均 ${avgDetail}ms），可适当调高并发数或检查目标站连通性。`;
    warn = true;
  } else if (avgArt >= 1500 && avgArt >= avgDetail) {
    hint = `主要耗时在下载封面图（平均 ${avgArt}ms），图床响应较慢，可按需关闭封面或提高并发。`;
    warn = true;
  } else if (avgCrop >= 800) {
    hint = `主要耗时在 FFmpeg 裁切海报（平均 ${avgCrop}ms），可改选「不裁切」加速。`;
  }
  return { count, avgTotal, avgDetail, avgTransfer, avgArt, avgCrop, crossFsCount, hint, warn };
});
function formatTimingLine(item: ScrapeItemOutcome) {
  const t = item.timing;
  if (!t) return '';
  const parts = [`总 ${t.totalMs}ms`, `详情 ${t.detailMs}ms`];
  if (t.transferMs > 0 || t.transferActual === 'copy-cross-fs') {
    parts.push(`文件 ${t.transferMs}ms${t.transferActual === 'copy-cross-fs' ? ' (跨卷复制)' : ''}`);
  }
  if (t.artworkMs > 0) parts.push(`封面 ${t.artworkMs}ms`);
  if (t.cropMs > 0) parts.push(`裁图 ${t.cropMs}ms`);
  if (item.failedStage) parts.push(`失败阶段: ${item.failedStage}`);
  return parts.join(' · ');
}
async function copyScrapeLogs() {
  const job = scrapeJob.value;
  if (!job) return;
  const lines: string[] = [];
  const statusText =
    job.status === 'running'
      ? '运行中'
      : job.status === 'done'
        ? '已完成'
        : job.status === 'stopped'
          ? '已停止'
          : job.status === 'error'
            ? '异常'
            : '空闲';
  lines.push(`StreamDock 后台刮削诊断 [${statusText}]: 已完成 ${job.done}/${job.total}，成功 ${job.succeeded}，失败 ${job.failed}`);
  if (scrapeTimingSummary.value) {
    const s = scrapeTimingSummary.value;
    lines.push(`平均耗时(${s.count}条): 总 ${s.avgTotal}ms | 详情 ${s.avgDetail}ms | 文件 ${s.avgTransfer}ms | 封面 ${s.avgArt}ms | 裁图 ${s.avgCrop}ms`);
    lines.push(`瓶颈建议: ${s.hint}`);
  }
  if (job.error) lines.push(`错误信息: ${job.error}`);
  for (const item of visibleScrapeOutcomes.value) {
    lines.push(
      `[${item.status === 'success' ? 'OK' : 'ERR'}] ${item.title || item.filePath} | ${formatTimingLine(item)}${item.status === 'failed' ? ` | ${item.message || ''}` : ''}`
    );
  }
  try {
    await navigator.clipboard.writeText(lines.join('\n'));
    notify('已复制刮削诊断日志');
  } catch {
    notify('浏览器未授予剪贴板权限，请手动查看页面或 docker logs', true);
  }
}

function cancelScan() {
  scrapeController?.abort();
  scrapeController = undefined;
  scrapeScanBusy.value = false;
}
function syncScanItemsWithPaths(paths: Iterable<string>) {
  if (!scrapeScanResult.value) return;
  const scanItemMap = new Map(scrapeScanResult.value.items.map(item => [item.filePath, item]));
  for (const filePath of paths) {
    scrapeSelected.delete(filePath);
    const scanned = scanItemMap.get(filePath);
    if (scanned) {
      scanned.hasNfo = true;
      scanned.status = 'skipped-nfo';
    }
  }
}
function applyScrapeJob(next: ScrapeJobSnapshot | null, fromWs = false) {
  if (fromWs) scrapeJobRevision++;
  const prev = scrapeJob.value;
  if (prev && next && prev.id === next.id && (next.revision ?? 0) < (prev.revision ?? 0)) {
    return;
  }
  const wasRunningSameJob = Boolean(prev && next && prev.id === next.id && prev.status === 'running');
  scrapeJob.value = next;
  if (!next) return;
  const donePaths = new Set<string>(next.succeededPaths || []);
  for (const item of next.results) {
    if (item.status === 'success') donePaths.add(item.filePath);
  }
  if (donePaths.size) syncScanItemsWithPaths(donePaths);
  if (wasRunningSameJob && next.status !== 'running') {
    if (next.status === 'done') {
      notify(`后台刮削完成：成功 ${next.succeeded}/${next.total}${next.failed ? `，失败 ${next.failed}` : ''}`, next.failed > 0);
    } else if (next.status === 'stopped') {
      notify(`已暂停后台刮削：已完成 ${next.done}/${next.total}（成功 ${next.succeeded}，失败 ${next.failed}），可随时点「继续剩余」`);
    } else if (next.status === 'error') {
      notify(next.error || '后台刮削异常终止', true);
    }
    if (scrapeScanResult.value && !next.succeededPaths && next.totalResults > next.results.length) {
      void api<ScrapeJobSnapshot | null>('scrape/job?includePaths=1')
        .then(full => {
          if (full?.succeededPaths?.length) syncScanItemsWithPaths(full.succeededPaths);
        })
        .catch(() => {});
    }
  }
}
function cleanScrapeScanDirs() {
  const seen = new Set<string>();
  const dirs: string[] = [];
  for (const raw of scrapeScanDirs.value) {
    const trimmed = raw.trim();
    if (!trimmed || seen.has(trimmed)) continue;
    seen.add(trimmed);
    dirs.push(trimmed);
  }
  return dirs;
}
function cleanScrapeSources(): ScrapeDataSource[] {
  const out: ScrapeDataSource[] = [];
  const seenIds = new Set<string>();
  for (const raw of scrapeSources.value) {
    const url = String(raw.url || '').trim();
    if (!url) continue;
    const type: ScrapeSourceType = raw.type === 'wordpress' ? 'wordpress' : 'rss';
    let id = String(raw.id || `${type}-${out.length + 1}`).trim();
    while (seenIds.has(id)) id = `${type}-${out.length + 1}-${seenIds.size}`;
    seenIds.add(id);
    out.push({
      id,
      name: String(raw.name || '').trim() || (type === 'wordpress' ? 'WordPress 站点' : 'RSS 数据源'),
      type,
      url,
      enabled: raw.enabled !== false,
      referer: String(raw.referer || '').trim(),
    });
    if (out.length >= 20) break;
  }
  return out;
}
function buildScrapeConfigPayload(): ScrapeConfig | null {
  const scanDirs = cleanScrapeScanDirs();
  const sources = cleanScrapeSources();
  if (!scanDirs.length || !sources.length) return null;
  const primaryRss = sources.find(src => src.enabled && src.type === 'rss') || sources[0];
  const minScore = Number.isFinite(scrapeMinScore.value) ? Math.min(100, Math.max(0, Math.round(scrapeMinScore.value))) : 55;
  const concurrency = Number.isFinite(scrapeConcurrency.value) ? Math.min(10, Math.max(1, Math.round(scrapeConcurrency.value))) : 3;
  return {
    scanDirs,
    sources,
    feedPreset: scrapeFeedPreset.value || 'custom',
    rssUrl: primaryRss?.url || scrapeRssUrl.value.trim(),
    referer: primaryRss?.referer || scrapeReferer.value.trim(),
    mode: scrapeMode.value,
    outputDir: scrapeOutputDir.value.trim(),
    transferMode: scrapeTransferMode.value,
    folderTemplate: scrapeFolderTemplate.value,
    posterCrop: scrapePosterCrop.value,
    minScore,
    recursive: scrapeRecursive.value,
    skipExistingNfo: scrapeSkipExistingNfo.value,
    downloadImages: scrapeDownloadImages.value,
    overwriteNfo: scrapeOverwriteNfo.value,
    concurrency,
  };
}
function applyScrapeConfig(cfg: ScrapeConfig) {
  const json = JSON.stringify(cfg);
  if (scrapeConfigHydrated && json === lastSavedScrapeConfigJson) return;
  hydratingScrapeConfig = true;
  scrapeScanDirs.value = cfg.scanDirs.length ? [...cfg.scanDirs] : [config.saveDir || './downloads'];
  scrapeSources.value =
    Array.isArray(cfg.sources) && cfg.sources.length
      ? cfg.sources.map(src => ({ ...src, referer: src.referer || '' }))
      : createDefaultScrapeSources();
  scrapeFeedPreset.value = cfg.feedPreset || 'custom';
  scrapeRssUrl.value = cfg.rssUrl ?? '';
  scrapeReferer.value = cfg.referer || '';
  scrapeMode.value = cfg.mode || 'in-place';
  scrapeOutputDir.value = cfg.outputDir || '';
  scrapeTransferMode.value = cfg.transferMode || 'move';
  scrapeFolderTemplate.value = cfg.folderTemplate || 'actor-title';
  scrapePosterCrop.value = cfg.posterCrop || 'center';
  scrapeMinScore.value = cfg.minScore ?? 55;
  scrapeRecursive.value = cfg.recursive !== false;
  scrapeSkipExistingNfo.value = cfg.skipExistingNfo !== false;
  scrapeDownloadImages.value = cfg.downloadImages !== false;
  scrapeOverwriteNfo.value = Boolean(cfg.overwriteNfo);
  scrapeConcurrency.value = cfg.concurrency ?? 3;
  lastSavedScrapeConfigJson = json;
  scrapeConfigHydrated = true;
  void nextTick().then(() => {
    hydratingScrapeConfig = false;
  });
}
async function flushSaveScrapeConfig() {
  if (hydratingScrapeConfig || !scrapeConfigHydrated || connection.value !== 'online') return;
  const payload = buildScrapeConfigPayload();
  if (!payload) return;
  const json = JSON.stringify(payload);
  if (json === lastSavedScrapeConfigJson) return;
  savingScrapeConfig = true;
  try {
    const saved = await api<ScrapeConfig>('scrape/config', payload);
    if (saved) lastSavedScrapeConfigJson = JSON.stringify(saved);
    scrapeConfigSavedAt.value = Date.now();
  } catch {
    /* Non-blocking background preference save. */
  } finally {
    savingScrapeConfig = false;
  }
}
function saveScrapeConfig(immediate = false) {
  if (hydratingScrapeConfig || !scrapeConfigHydrated) return Promise.resolve();
  clearTimeout(scrapeConfigTimer);
  scrapeConfigTimer = undefined;
  if (immediate) return flushSaveScrapeConfig();
  scrapeConfigTimer = setTimeout(() => {
    scrapeConfigTimer = undefined;
    void flushSaveScrapeConfig();
  }, 400);
  return Promise.resolve();
}
watch(
  [
    scrapeScanDirs,
    scrapeSources,
    scrapeFeedPreset,
    scrapeRssUrl,
    scrapeReferer,
    scrapeMode,
    scrapeOutputDir,
    scrapeTransferMode,
    scrapeFolderTemplate,
    scrapePosterCrop,
    scrapeMinScore,
    scrapeRecursive,
    scrapeSkipExistingNfo,
    scrapeDownloadImages,
    scrapeOverwriteNfo,
    scrapeConcurrency,
  ],
  () => {
    void saveScrapeConfig(false);
  },
  { deep: true }
);
function addScrapeScanDir() {
  if (scrapeScanDirs.value.length >= 20) return;
  scrapeScanDirs.value.push('');
}
function removeScrapeScanDir(index: number) {
  if (scrapeScanDirs.value.length <= 1) return;
  scrapeScanDirs.value.splice(index, 1);
  void saveScrapeConfig(true);
}
function addScrapeSource(type: ScrapeSourceType) {
  if (scrapeSources.value.length >= 20) return;
  scrapeSources.value.push({
    id: `${type}-${Date.now()}-${scrapeSources.value.length + 1}`,
    name: type === 'wordpress' ? '新 WordPress 站点' : '新 RSS 数据源',
    type,
    url: '',
    enabled: true,
    referer: '',
  });
}
function importRssSourceToScrape() {
  const feed = rssSources.value.find(item => item.id === scrapeImportFeedId.value);
  scrapeImportFeedId.value = '';
  if (!feed || scrapeSources.value.length >= 20) return;
  if (scrapeSources.value.some(src => src.url.trim() === feed.url.trim())) {
    notify('该订阅源已在元数据列表中');
    return;
  }
  scrapeSources.value.push({
    id: `rss-${feed.id}`,
    name: feed.name || feed.id,
    type: 'rss',
    url: feed.url,
    enabled: true,
    referer: feed.referer || '',
  });
  void saveScrapeConfig(true);
}
function moveScrapeSource(index: number, delta: -1 | 1) {
  const target = index + delta;
  if (target < 0 || target >= scrapeSources.value.length) return;
  const next = [...scrapeSources.value];
  const [moved] = next.splice(index, 1);
  next.splice(target, 0, moved);
  scrapeSources.value = next;
  void saveScrapeConfig(true);
}
function removeScrapeSource(index: number) {
  if (scrapeSources.value.length <= 1) return;
  scrapeSources.value.splice(index, 1);
  void saveScrapeConfig(true);
}
async function stopScrape() {
  if (scrapeStopBusy.value) return;
  scrapeStopBusy.value = true;
  scrapeError.value = '';
  try {
    const job = await api<ScrapeJobSnapshot | null>('scrape/job/stop', {});
    applyScrapeJob(job);
  } catch (error) {
    scrapeError.value = (error as Error).message;
  } finally {
    scrapeStopBusy.value = false;
  }
}
async function resumeScrape(retryFailed = false) {
  if (scrapeRunBusy.value || scrapeScanBusy.value || !scrapeJob.value) return;
  if (scrapeMode.value === 'organize' && !scrapeOutputDir.value.trim()) {
    scrapeError.value = '按目录归档时请填写目标输出目录';
    return;
  }
  scrapeResumeBusy.value = true;
  scrapeError.value = '';
  try {
    await saveScrapeConfig(true);
    const job = await api<ScrapeJobSnapshot>('scrape/job/resume', {
      retryFailed,
      mode: scrapeMode.value,
      outputDir: scrapeMode.value === 'organize' ? scrapeOutputDir.value.trim() : undefined,
      transferMode: scrapeTransferMode.value,
      folderTemplate: scrapeFolderTemplate.value,
      posterCrop: scrapePosterCrop.value,
      downloadImages: scrapeDownloadImages.value,
      overwriteNfo: scrapeOverwriteNfo.value,
      concurrency: scrapeConcurrency.value,
    });
    applyScrapeJob(job);
    notify(retryFailed ? '已开始重试失败视频' : `已继续后台刮削剩余 ${job.remaining} 个视频`);
  } catch (error) {
    scrapeError.value = (error as Error).message;
    try {
      const current = await api<ScrapeJobSnapshot | null>('scrape/job');
      applyScrapeJob(current);
    } catch {
      /* Preserve existing scrapeError. */
    }
  } finally {
    scrapeResumeBusy.value = false;
  }
}
async function clearScrapeJob() {
  if (scrapeRunBusy.value) return;
  scrapeError.value = '';
  try {
    await api<null>('scrape/job/clear', {});
    scrapeJob.value = null;
  } catch (error) {
    scrapeError.value = (error as Error).message;
  }
}
async function scanScrape(refreshRss = false) {
  if (scrapeScanBusy.value || scrapeRunBusy.value) return;
  const scanDirs = cleanScrapeScanDirs();
  if (!scanDirs.length) {
    scrapeError.value = '请至少填写一个扫描目录';
    return;
  }
  const sources = cleanScrapeSources();
  if (!sources.some(src => src.enabled)) {
    scrapeError.value = '请至少启用一个已填写地址的数据源';
    return;
  }
  cancelScan();
  const controller = new AbortController();
  scrapeController = controller;
  scrapeScanBusy.value = true;
  scrapeError.value = '';
  void saveScrapeConfig(true);
  try {
    const primaryRss = sources.find(src => src.enabled && src.type === 'rss') || sources[0];
    const data = await api<ScrapeScanResult>(
      'scrape/scan',
      {
        scanDir: scanDirs[0],
        scanDirs,
        sources,
        rssUrl: primaryRss?.url || scrapeRssUrl.value.trim(),
        recursive: scrapeRecursive.value,
        skipExistingNfo: scrapeSkipExistingNfo.value,
        minScore: scrapeMinScore.value,
        refreshRss,
      },
      controller.signal
    );
    if (controller.signal.aborted) return;
    scrapeScanResult.value = data;
    scrapeSelected.clear();
    for (const key of Object.keys(scrapeChoice)) delete scrapeChoice[key];
    for (const item of data.items) {
      const topUrl = item.match?.pageUrl || item.candidates[0]?.pageUrl || '';
      scrapeChoice[item.filePath] = topUrl;
      if (item.status === 'matched' && topUrl) scrapeSelected.add(item.filePath);
    }
    notify(`扫描完成：发现 ${data.scannedFiles} 个视频，高置信匹配 ${data.matchedCount} 个`);
  } catch (error) {
    if (!controller.signal.aborted) scrapeError.value = (error as Error).message;
  } finally {
    if (scrapeController === controller) {
      scrapeController = undefined;
      scrapeScanBusy.value = false;
    }
  }
}
async function executeScrape() {
  if (scrapeRunBusy.value || scrapeScanBusy.value || !scrapeScanResult.value) return;
  const payloadItems: ScrapeRunItemInput[] = [];
  for (const item of scrapeScanResult.value.items) {
    if (!scrapeSelected.has(item.filePath)) continue;
    const chosenUrl = scrapeChoice[item.filePath];
    if (!chosenUrl) continue;
    const candidate = item.candidates.find(c => c.pageUrl === chosenUrl) || item.match;
    payloadItems.push({
      filePath: item.filePath,
      pageUrl: chosenUrl,
      title: candidate?.title,
      pubDate: candidate?.pubDate,
      tags: candidate?.tags,
      guid: candidate?.guid,
      actors: candidate?.actors,
      studio: candidate?.studio,
      coverUrl: candidate?.coverUrl,
      referer: candidate?.referer,
    });
  }
  if (!payloadItems.length) {
    scrapeError.value = '请至少勾选一个已匹配详情页的视频';
    return;
  }
  if (scrapeMode.value === 'organize' && !scrapeOutputDir.value.trim()) {
    scrapeError.value = '按目录归档时请填写目标输出目录';
    return;
  }
  scrapeStartBusy.value = true;
  scrapeError.value = '';
  void saveScrapeConfig(true);
  try {
    const job = await api<ScrapeJobSnapshot>('scrape/job', {
      items: payloadItems,
      mode: scrapeMode.value,
      outputDir: scrapeMode.value === 'organize' ? scrapeOutputDir.value.trim() : undefined,
      transferMode: scrapeTransferMode.value,
      folderTemplate: scrapeFolderTemplate.value,
      posterCrop: scrapePosterCrop.value,
      downloadImages: scrapeDownloadImages.value,
      overwriteNfo: scrapeOverwriteNfo.value,
      concurrency: scrapeConcurrency.value,
    });
    applyScrapeJob(job);
    notify(`已启动后台刮削：共 ${payloadItems.length} 个视频，关闭页面仍会继续`);
  } catch (error) {
    scrapeError.value = (error as Error).message;
    try {
      const current = await api<ScrapeJobSnapshot | null>('scrape/job');
      applyScrapeJob(current);
    } catch {
      /* Preserve existing scrapeError. */
    }
  } finally {
    scrapeStartBusy.value = false;
  }
}

async function retryTaskScrape(task: TaskItem) {
  if (pending.has(task.url)) return;
  pending.add(task.url);
  try {
    const updated = await api<TaskItem>('tasks/scrape', { url: task.url, overwriteNfo: true });
    if (updated) tasks.value[task.url] = updated;
    notify(updated?.scrapeStatus === 'done' ? 'NFO 与封面生成完成' : updated?.scrapeMessage || '刮削完成', updated?.scrapeStatus === 'error');
  } catch (error) {
    notify((error as Error).message, true);
  } finally {
    pending.delete(task.url);
  }
}

const rssTotalPages = computed(() => Math.max(1, Math.ceil((rssItemsPage.value?.total || 0) / Math.max(1, rssPageSize.value))));
const allRssSelected = computed(() => {
  const items = rssItemsPage.value?.items || [];
  return items.length > 0 && items.every(item => rssSelectedGuids.has(item.guid));
});

async function loadRssItems() {
  const source = activeRssSource.value;
  if (!source) {
    rssItemsPage.value = null;
    return;
  }
  const seq = ++rssItemsSeq;
  rssItemsLoading.value = true;
  rssItemsError.value = '';
  try {
    const params = new URLSearchParams({
      page: String(rssPage.value),
      pageSize: String(rssPageSize.value),
      status: rssStatusFilter.value,
      sort: rssSort.value,
    });
    if (rssSearch.value.trim()) params.set('q', rssSearch.value.trim());
    const data = await api<RssItemsPage>(`rss/sources/${encodeURIComponent(source.id)}/items?${params.toString()}`);
    if (seq !== rssItemsSeq) return;
    rssItemsPage.value = data;
    rssPage.value = data.page;
  } catch (error) {
    if (seq === rssItemsSeq) rssItemsError.value = (error as Error).message;
  } finally {
    if (seq === rssItemsSeq) rssItemsLoading.value = false;
  }
}

watch(rssSearch, () => {
  clearTimeout(rssSearchTimer);
  rssSearchTimer = setTimeout(() => {
    rssPage.value = 1;
    rssSelectedGuids.clear();
    if (view.value === 'rss' && activeRssSource.value) void loadRssItems();
  }, 250);
});

function openRss(mobileRoot = false) {
  view.value = 'rss';
  if (mobileRoot) rssDetailOpen.value = false;
  if (activeRssSource.value && (!rssItemsPage.value || rssItemsPage.value.sourceId !== activeRssSource.value.id)) {
    void loadRssItems();
  }
}

function selectRssSource(sourceId: string) {
  rssSelectedId.value = sourceId;
  rssPage.value = 1;
  rssStatusFilter.value = 'all';
  rssSelectedGuids.clear();
  rssDetailOpen.value = true;
  void loadRssItems();
}

function setRssStatusFilter(next: 'all' | 'new' | 'downloaded' | 'ignored' | 'filtered' | 'seed-skipped') {
  rssStatusFilter.value = next;
  rssPage.value = 1;
  rssSelectedGuids.clear();
  void loadRssItems();
}

function toggleRssSort() {
  rssSort.value = rssSort.value === 'newest' ? 'oldest' : 'newest';
  rssPage.value = 1;
  void loadRssItems();
}

function changeRssPage(nextPage: number) {
  rssPage.value = Math.min(rssTotalPages.value, Math.max(1, nextPage));
  rssSelectedGuids.clear();
  void loadRssItems();
}

function onRssPageSizeChange() {
  rssPage.value = 1;
  rssSelectedGuids.clear();
  void loadRssItems();
}

function toggleRssGuid(guid: string) {
  if (rssSelectedGuids.has(guid)) rssSelectedGuids.delete(guid);
  else rssSelectedGuids.add(guid);
}

function toggleAllRssItems() {
  const items = rssItemsPage.value?.items || [];
  if (allRssSelected.value) {
    for (const item of items) rssSelectedGuids.delete(item.guid);
  } else {
    for (const item of items) rssSelectedGuids.add(item.guid);
  }
}

async function refreshActiveRss() {
  const source = activeRssSource.value;
  if (!source || rssRefreshingId.value) return;
  rssRefreshingId.value = source.id;
  try {
    const res = await api<RssRefreshResult>(`rss/sources/${encodeURIComponent(source.id)}/refresh`, {});
    notify(`刷新完成：共 ${res.totalItems} 条（新增 ${res.added} 条${res.autoQueued ? `，自动入队 ${res.autoQueued}` : ''}）`);
    await load();
    await loadRssItems();
  } catch (error) {
    notify((error as Error).message, true);
  } finally {
    rssRefreshingId.value = '';
  }
}

async function markAllRssRead() {
  const source = activeRssSource.value;
  if (!source || rssActionBusy.value) return;
  rssActionBusy.value = true;
  try {
    await api(`rss/sources/${encodeURIComponent(source.id)}/mark-all-read`, {});
    rssSelectedGuids.clear();
    notify('已将该源新条目全部设为已忽略');
    await load();
    await loadRssItems();
  } catch (error) {
    notify((error as Error).message, true);
  } finally {
    rssActionBusy.value = false;
  }
}

async function actRssItems(guids: string[], action: 'download' | 'ignore' | 'mark-new') {
  const source = activeRssSource.value;
  if (!source || !guids.length || rssActionBusy.value) return;
  rssActionBusy.value = true;
  try {
    const res = await api<RssActionResult>(`rss/sources/${encodeURIComponent(source.id)}/actions`, { guids, action });
    for (const guid of guids) rssSelectedGuids.delete(guid);
    if (action === 'download') {
      const msg = `已加入 ${res.queued} 个下载任务${res.skipped ? `，跳过 ${res.skipped} 个` : ''}${res.errors.length ? `，失败 ${res.errors.length} 个` : ''}`;
      notify(res.errors[0]?.message && !res.queued ? res.errors[0].message : msg, res.errors.length > 0 && res.queued === 0);
    } else {
      notify(`已更新 ${res.updated} 条订阅状态`);
    }
    await load();
    await loadRssItems();
  } catch (error) {
    notify((error as Error).message, true);
  } finally {
    rssActionBusy.value = false;
  }
}

async function openRssEditor(source: RssSource | null) {
  rssEditingSource.value = source;
  rssEditorTab.value = 'basic';
  rssValidateResult.value = null;
  rssValidateError.value = '';
  modalError.value = '';
  Object.assign(rssForm, {
    mode: source?.mode || 'rss',
    url: source?.url || '',
    name: source?.name || '',
    enabled: source?.enabled ?? true,
    autoDownload: source?.autoDownload ?? false,
    intervalMinutes: source?.intervalMinutes ?? 30,
    maxPerFetch: source?.maxPerFetch ?? 20,
    itemPattern: source?.itemPattern || '',
    includePattern: source?.includePattern || '',
    excludePattern: source?.excludePattern || '',
    useRegex: source?.useRegex ?? false,
    smartEpisode: source?.smartEpisode ?? false,
    sizeMinText: formatBytesInput(source?.sizeMinBytes || 0),
    sizeMaxText: formatBytesInput(source?.sizeMaxBytes || 0),
    saveDir: source?.saveDir || '',
    referer: source?.referer || '',
    cookies: source?.cookies || '',
    userAgent: source?.userAgent || '',
    autoScrape: source?.autoScrape ?? true,
    scrapeMode: source?.scrapeMode || 'in-place',
    scrapeOutputDir: source?.scrapeOutputDir || '',
    scrapeTransferMode: source?.scrapeTransferMode || 'move',
    scrapeFolderTemplate: source?.scrapeFolderTemplate || 'actor-title',
    scrapePosterCrop: source?.scrapePosterCrop || 'center',
    scrapeDownloadImages: source?.scrapeDownloadImages ?? true,
  });
  rssEditorOpen.value = true;
  await openModal();
}

async function openDeleteRss(source: RssSource) {
  rssDeleteSource.value = source;
  modalError.value = '';
  await openModal();
}

async function validateRssUrl() {
  if (rssValidateBusy.value) return;
  rssValidateBusy.value = true;
  rssValidateError.value = '';
  try {
    const res = await api<RssValidateResult>('rss/validate', {
      url: rssForm.url.trim(),
      mode: rssForm.mode,
      itemPattern: rssForm.itemPattern.trim(),
      referer: rssForm.referer.trim(),
      cookies: rssForm.cookies.trim(),
      userAgent: rssForm.userAgent.trim(),
    });
    rssValidateResult.value = res;
    if (!rssForm.name.trim() && res.feedTitle) rssForm.name = res.feedTitle;
  } catch (error) {
    rssValidateError.value = (error as Error).message;
  } finally {
    rssValidateBusy.value = false;
  }
}

function rssSourceSubline(source: RssSource) {
  if (source.lastError) return `异常：${source.lastError}`;
  if (!source.enabled) return `已停用 · 共 ${source.totalCount || 0} 条`;
  const modeText = source.autoDownload ? `每 ${source.intervalMinutes} 分钟自动下载` : '收集模式';
  return `共 ${source.totalCount || 0} 条 · ${modeText}`;
}

function rssItemStatusLabel(item: RssFeedItem) {
  const linked = item.taskUrl ? tasks.value[item.taskUrl] : undefined;
  if (linked) {
    if (linked.status === 'done') {
      if (linked.scrapeStatus === 'done') return { text: '已完成 · NFO 已生成', tone: 'done' };
      if (linked.scrapeStatus === 'scraping') return { text: '已完成 · 生成 NFO 中', tone: 'active' };
      if (linked.scrapeStatus === 'error') return { text: '已完成 · NFO 失败', tone: 'warn' };
      return { text: '已下载完成', tone: 'done' };
    }
    if (linked.status === 'resume') return { text: linked.phase === 'merging' ? '正在合并' : `下载中 ${progress(linked)}%`, tone: 'active' };
    if (linked.status === 'pending') return { text: '排队中', tone: 'active' };
    if (linked.status === 'pause') return { text: '已暂停', tone: 'warn' };
    if (linked.status === 'error') return { text: '下载失败', tone: 'error' };
  }
  switch (item.status) {
    case 'new':
      return { text: '新条目', tone: 'new' };
    case 'downloaded':
      return { text: '已入队', tone: 'done' };
    case 'ignored':
      return { text: '已忽略', tone: 'muted' };
    case 'filtered':
      return { text: '已过滤', tone: 'warn' };
    case 'duplicate-episode':
      return { text: '重复剧集', tone: 'warn' };
    case 'seed-skipped':
      return { text: '历史跳过', tone: 'muted' };
  }
}

function rssReasonLabel(item: RssFeedItem) {
  switch (item.reason) {
    case 'not_included':
      return '未命中包含规则';
    case 'excluded':
      return '命中排除规则';
    case 'too_small':
      return '小于体积下限';
    case 'too_large':
      return '超过体积上限';
    case 'dup_episode':
      return item.episodeKey ? `同集 ${item.episodeKey} 已下载` : '重复剧集';
    case 'seed_skipped':
      return '开启自动下载前的历史条目';
    default:
      return '';
  }
}

function logout() {
  clearSavedToken();
  token.value = '';
  tasks.value = {};
  rssSources.value = [];
  rssItemsPage.value = null;
  scrapeJob.value = null;
  scrapeScanResult.value = null;
  scrapeSelected.clear();
  clearTimeout(scrapeConfigTimer);
  scrapeConfigTimer = undefined;
  scrapeConfigHydrated = false;
  lastSavedScrapeConfigJson = '';
  scrapeScanDirs.value = ['./downloads'];
  socket?.close();
  view.value = 'tasks';
  closeModal();
  loginOpen.value = true;
  void openModal();
  notify('已退出当前浏览器登录');
}
function fileUrl(task: TaskItem) {
  return `/api/file?url=${encodeURIComponent(task.url)}&token=${encodeURIComponent(token.value)}`;
}
function onVisible() {
  if (!document.hidden) {
    void load();
    if (connection.value !== 'online') connect();
  }
}
onMounted(() => {
  void load();
  connect();
  document.addEventListener('visibilitychange', onVisible);
  mediaQuery?.addEventListener('change', onSystemThemeChange);
});
onBeforeUnmount(() => {
  stopped = true;
  cancelExtraction();
  cancelScan();
  clearTimeout(scrapeConfigTimer);
  clearInterval(segmentTimer);
  clearTimeout(reconnectTimer);
  clearTimeout(noticeTimer);
  clearTimeout(rssSearchTimer);
  socket?.close();
  document.documentElement.classList.remove('modal-open');
  document.removeEventListener('visibilitychange', onVisible);
  mediaQuery?.removeEventListener('change', onSystemThemeChange);
});
</script>

<template>
  <div class="app-shell">
    <a class="skip-link" href="#main-content">跳到主内容</a>

    <header class="zen-header">
      <div class="header-left">
        <a class="brand" href="/" aria-label="StreamDock 首页">
          <span class="zen-icon zen-icon-blue"><AppIcon name="bolt" :size="20" /></span>
          <strong>Stream<span>Dock</span></strong>
        </a>
        <span class="zen-version-pill" :title="`StreamDock v${appVersion}`">v{{ appVersion }}</span>
        <div class="zen-status-pill" :class="connection" role="status">
          <i />
          <span>{{ connection === 'online' ? '服务已连接' : connection === 'connecting' ? '正在连接' : '服务未连接' }}</span>
        </div>
      </div>

      <div class="header-actions">
        <button
          type="button"
          class="zen-mode-toggle"
          :aria-label="resolvedTheme === 'dark' ? '切换到浅色模式' : '切换到深色模式'"
          @click="toggleTheme"
        >
          <AppIcon :name="resolvedTheme === 'dark' ? 'sun' : 'moon'" :size="16" />
          <span>{{ resolvedTheme === 'dark' ? '浅色' : '深色' }}</span>
        </button>
        <button type="button" class="button secondary extract-button" aria-label="打开网页提取" @click="newDownload('extract')">
          <AppIcon name="link" :size="16" />
          <span>网页提取</span>
        </button>
        <button type="button" class="button primary new-button" @click="newDownload('download')">
          <AppIcon name="plus" :size="16" />
          <span>新建下载</span>
        </button>
      </div>
    </header>

    <nav class="zc-app-nav" aria-label="主导航">
      <div class="zc-nav-links">
        <button type="button" class="zc-nav-item" :aria-current="view === 'tasks' ? 'page' : undefined" @click="view = 'tasks'">
          <span class="zen-icon zen-nav-icon zen-icon-blue"><AppIcon name="layers" :size="16" /></span>
          <span>任务概览</span>
          <span v-if="active.length" class="zc-nav-badge">{{ active.length }}</span>
        </button>
        <button type="button" class="zc-nav-item" :aria-current="view === 'rss' ? 'page' : undefined" @click="openRss(false)">
          <span class="zen-icon zen-nav-icon zen-icon-orange"><AppIcon name="rss" :size="16" /></span>
          <span>RSS 订阅</span>
          <span v-if="rssUnreadTotal" class="zc-nav-badge">{{ rssUnreadTotal }}</span>
        </button>
        <button type="button" class="zc-nav-item" :aria-current="view === 'scrape' ? 'page' : undefined" @click="view = 'scrape'">
          <span class="zen-icon zen-nav-icon zen-icon-teal"><AppIcon name="film" :size="16" /></span>
          <span>媒体刮削</span>
          <span v-if="scrapeJob?.status === 'running'" class="zc-nav-badge">{{ scrapeJob.done }}/{{ scrapeJob.total }}</span>
        </button>
        <button type="button" class="zc-nav-item" :aria-current="view === 'settings' ? 'page' : undefined" @click="openSettings()">
          <span class="zen-icon zen-nav-icon zen-icon-purple"><AppIcon name="settings" :size="16" /></span>
          <span>系统设置</span>
        </button>
      </div>
      <a class="zc-nav-aside" href="/scriptcat.user.js" target="_blank" rel="noopener noreferrer">
        <AppIcon name="script" :size="15" />
        <span>脚本猫脚本</span>
      </a>
    </nav>

    <main id="main-content" class="workspace" tabindex="-1">
      <div v-if="connection === 'offline' && !loginOpen" class="offline-banner" role="status">
        <AppIcon name="info" :size="16" />
        <span>服务连接已断开，正在自动重连。</span>
        <button type="button" @click="connect(); load()">重新连接</button>
      </div>

      <template v-if="view === 'tasks'">
        <h1 class="zc-sr-only">下载任务概览</h1>

        <section class="zen-metrics" aria-label="运行指标">
          <div class="zen-metric-card">
            <div class="zen-metric-head">
              <span class="zen-icon zen-small-icon zen-icon-blue"><AppIcon name="activity" :size="16" /></span>
              <span>实时速率</span>
            </div>
            <strong>{{ bytes(speed) }}<small>/s</small></strong>
            <span class="zen-metric-sub">{{ running }} / {{ config.maxDownloads }} 并发运行</span>
            <div class="zen-metric-track"><i :style="{ width: `${runningRatio}%` }" /></div>
          </div>

          <button type="button" class="zen-metric-card interactive" :class="{ selected: tab === 'active' }" @click="tab = 'active'">
            <div class="zen-metric-head">
              <span class="zen-icon zen-small-icon zen-icon-teal"><AppIcon name="layers" :size="16" /></span>
              <span>进行中</span>
            </div>
            <strong>{{ active.length }}</strong>
            <span class="zen-metric-sub">{{ running }} 运行 · {{ Math.max(0, active.length - running) }} 等待或暂停</span>
          </button>

          <button type="button" class="zen-metric-card interactive" :class="{ selected: tab === 'done' }" @click="tab = 'done'">
            <div class="zen-metric-head">
              <span class="zen-icon zen-small-icon zen-icon-green"><AppIcon name="check" :size="16" /></span>
              <span>已完成</span>
            </div>
            <strong>{{ complete.length }}</strong>
            <span class="zen-metric-sub">合计 {{ bytes(completedBytes) }}</span>
          </button>

          <button type="button" class="zen-metric-card interactive" :class="{ selected: tab === 'error', danger: errors > 0 }" @click="tab = 'error'">
            <div class="zen-metric-head">
              <span class="zen-icon zen-small-icon zen-icon-orange"><AppIcon name="alert" :size="16" /></span>
              <span>需要重试</span>
            </div>
            <strong>{{ errors }}</strong>
            <span class="zen-metric-sub">{{ failedSegmentTasks.length ? `${failedSegmentTasks.length} 个可重试失败分片` : errors ? '查看失败原因并重试' : '全部任务正常' }}</span>
          </button>
        </section>

        <section id="task-panel" class="zen-panel" role="tabpanel" :aria-labelledby="`${tab}-tab`" :aria-busy="loading">
          <div class="zen-panel-head">
            <div class="zen-panel-title">
              <span class="zen-icon zen-section-icon zen-icon-blue"><AppIcon name="arrow" :size="18" /></span>
              <div>
                <h2>下载任务</h2>
                <p>视频、字幕与网页媒体集中管理</p>
              </div>
            </div>

            <div class="zen-panel-controls">
              <div class="zen-segmented" role="tablist" aria-label="任务分类" @keydown="navigateTabs">
                <button id="active-tab" type="button" role="tab" :tabindex="tab === 'active' ? 0 : -1" :aria-selected="tab === 'active'" :class="{ 'is-active': tab === 'active' }" aria-controls="task-panel" @click="tab = 'active'">进行中 <span>{{ active.length }}</span></button>
                <button id="done-tab" type="button" role="tab" :tabindex="tab === 'done' ? 0 : -1" :aria-selected="tab === 'done'" :class="{ 'is-active': tab === 'done' }" aria-controls="task-panel" @click="tab = 'done'">已完成 <span>{{ complete.length }}</span></button>
                <button id="error-tab" type="button" role="tab" :tabindex="tab === 'error' ? 0 : -1" :aria-selected="tab === 'error'" :class="{ 'is-active': tab === 'error' }" aria-controls="task-panel" @click="tab = 'error'">失败 <span>{{ errors }}</span></button>
              </div>
              <label class="zen-search">
                <AppIcon name="search" :size="16" />
                <input v-model="search" type="search" placeholder="搜索文件名或链接" aria-label="搜索下载任务" />
              </label>
            </div>
          </div>

          <fieldset class="zen-toolbar" :class="{ 'has-selection': selectedTasks.length }">
            <legend class="sr-only">批量操作</legend>
            <label class="task-checkbox select-all">
              <input type="checkbox" :checked="allSelected" :indeterminate="selectedTasks.length > 0 && !allSelected" :disabled="!visible.length || batchBusy || loading || pending.size > 0" aria-label="全选当前列表" @change="toggleAll" />
              <span>{{ selectedTasks.length ? `已选 ${selectedTasks.length} 项` : '全选' }}</span>
            </label>

            <template v-if="selectedTasks.length">
              <div class="selection-actions">
                <button v-if="tab !== 'done'" type="button" class="toolbar-button" :disabled="batchBusy || !batchTargets('resume', true).length" @click="batch('resume', true)"><AppIcon name="resume" :size="15" />继续 / 重试 <span>{{ batchTargets('resume', true).length }}</span></button>
                <button v-if="tab !== 'done'" type="button" class="toolbar-button" :disabled="batchBusy || !batchTargets('pause', true).length" @click="batch('pause', true)"><AppIcon name="pause" :size="15" />暂停 <span>{{ batchTargets('pause', true).length }}</span></button>
                <button v-if="batchTargets('retry-failed', true).length" type="button" class="toolbar-button retry-all" :disabled="batchBusy" @click="batch('retry-failed', true)"><AppIcon name="retry" :size="15" />仅重试失败分片</button>
                <button type="button" class="toolbar-button remove-button" :disabled="batchBusy || pending.size > 0" @click="remove(selectedTasks)"><AppIcon name="trash" :size="15" />移除所选</button>
              </div>
              <span v-if="batchBusy" class="batch-progress" role="status"><span class="spinner mini" />处理中…</span>
              <button type="button" class="toolbar-button clear-selection" :disabled="batchBusy" @click="selected.clear()"><AppIcon name="close" :size="14" />取消选择</button>
            </template>

            <template v-else>
              <span class="selection-hint">勾选任务以批量操作</span>
              <div class="batch-actions">
                <template v-if="tab !== 'done'">
                  <button type="button" class="toolbar-button" :disabled="!batchTargets('resume').length || batchBusy" @click="batch('resume')"><AppIcon name="resume" :size="15" />全部继续</button>
                  <button type="button" class="toolbar-button" :disabled="!batchTargets('pause').length || batchBusy" @click="batch('pause')"><AppIcon name="pause" :size="15" />全部暂停</button>
                  <button v-if="failedSegmentTasks.length" type="button" class="toolbar-button retry-all" :disabled="batchBusy" @click="batch('retry-failed')"><AppIcon name="retry" :size="15" />重试失败分片</button>
                </template>
                <span v-else class="completed-note"><AppIcon name="check" :size="15" />可保存至当前设备</span>
              </div>
            </template>
          </fieldset>

          <div class="zen-task-list">
            <div v-if="loading" class="loading-state" role="status">
              <div v-for="n in 3" :key="n" class="zen-skeleton-card"><span /><span /><span /></div>
              <p>正在同步下载任务…</p>
            </div>

            <div v-else-if="!visible.length" class="zen-empty">
              <span class="zen-icon zen-icon-blue zen-empty-icon"><AppIcon :name="search ? 'search' : 'arrow'" :size="24" /></span>
              <h3>{{ search ? '没有找到相关任务' : tab === 'done' ? '暂无已完成的下载' : tab === 'error' ? '没有失败的任务' : '暂无下载任务' }}</h3>
              <p>{{ search ? '试试其他文件名或链接' : tab === 'done' ? '下载完成的文件会显示在这里' : tab === 'error' ? '下载失败时，可在这里仅重试失败的分片' : '添加 M3U8、视频或字幕直链，开始下载' }}</p>
              <button v-if="!search && tab === 'active'" type="button" class="button primary" @click="newDownload('download')"><AppIcon name="plus" :size="16" />新建下载</button>
              <button v-else-if="search" type="button" class="button secondary" @click="search = ''">清除搜索</button>
            </div>

            <article
              v-for="task in visible"
              v-else
              :key="task.url"
              class="zen-task-card"
              :class="{ completed: task.status === 'done', failed: task.status === 'error', 'row-selected': selected.has(task.url) }"
            >
              <div class="task-main">
                <label class="task-checkbox">
                  <input type="checkbox" :checked="selected.has(task.url)" :disabled="batchBusy || pending.has(task.url)" :aria-label="`选择 ${name(task)}`" @change="toggleTask(task.url)" />
                  <span class="sr-only">选择 {{ name(task) }}</span>
                </label>
                <span class="zen-format-badge" :class="`tone-${formatTone(task)}`">{{ kind(task) }}</span>
                <div class="file-description">
                  <button type="button" class="file-name" :title="name(task)" @click="details(task)">{{ name(task) }}</button>
                  <span class="file-source">{{ taskSource(task) }}<template v-if="task.startTime"> · {{ formatTime(task.endTime || task.startTime) }}</template></span>
                </div>
              </div>

              <div class="task-progress">
                <div class="progress-line">
                  <div class="progress-track" role="progressbar" :aria-label="`${name(task)} ${label(task)}`" :aria-valuenow="progress(task)" aria-valuemin="0" aria-valuemax="100">
                    <div :style="{ width: `${progress(task)}%` }" />
                  </div>
                  <span>{{ progress(task) }}%</span>
                </div>
                <div v-if="isFile(task)" class="fragment-link static">
                  <AppIcon name="file" :size="13" />
                  <span>{{ bytes(task.downloadedSize) }} / {{ task.size ? bytes(task.size) : '总大小未知' }}</span>
                </div>
                <button v-else type="button" class="fragment-link" :aria-label="detailLabel(task)" @click="details(task)">
                  <AppIcon name="info" :size="13" />
                  <span>{{ task.tsSuccess || 0 }}/{{ task.tsCount || '—' }} 分片</span>
                  <b v-if="task.tsFailed" class="failure-number">{{ task.tsFailed }} 失败</b>
                  <span class="progress-size">· {{ bytes(task.downloadedSize) }}</span>
                </button>
                <p v-if="task.status === 'error' && task.errmsg" class="task-error" :title="task.errmsg">{{ task.errmsg }}</p>
                <p v-else-if="task.phase === 'merging' && task.status === 'resume'" class="task-merge-note">{{ task.mergeMessage || '等待合并' }}</p>
              </div>

              <div class="task-tail">
                <div class="task-telemetry">
                  <span v-if="task.status === 'resume' && task.phase === 'downloading'" class="zen-rate-pill">
                    ↓ {{ task.speedDesc || '0 B/s' }} · 剩余 {{ eta(task.remainingTime) }}
                  </span>
                  <span
                    v-if="task.status === 'done' && task.scrapeStatus && task.scrapeStatus !== 'idle'"
                    class="zen-nfo-pill"
                    :class="task.scrapeStatus"
                    :title="task.scrapeStatus === 'done' ? task.nfoPath : task.scrapeStatus === 'error' ? task.scrapeMessage : '正在刮削 NFO 与封面'"
                  >
                    <span v-if="task.scrapeStatus === 'scraping'" class="spinner mini" />
                    {{ task.scrapeStatus === 'done' ? 'NFO 已生成' : task.scrapeStatus === 'error' ? 'NFO 失败' : '生成 NFO 中' }}
                  </span>
                  <span class="zen-status" :class="[task.status, { merging: task.phase === 'merging' && task.status === 'resume' }]">
                    <span v-if="task.status === 'resume' && task.phase === 'merging'" class="spinner mini" />
                    {{ label(task) }}
                  </span>
                </div>

                <div class="task-actions">
                  <button type="button" class="icon-button" :aria-label="detailLabel(task)" :title="isFile(task) ? '下载详情' : '分片详情'" @click="details(task)"><AppIcon name="info" :size="17" /></button>
                  <button
                    v-if="task.status === 'done' && task.scrapeMeta?.pageUrl"
                    type="button"
                    class="icon-button"
                    :disabled="pending.has(task.url) || task.scrapeStatus === 'scraping'"
                    :aria-label="`重新刮削 ${name(task)} 的 NFO`"
                    title="重刮 NFO"
                    @click="retryTaskScrape(task)"
                  ><AppIcon name="film" :size="16" /></button>
                  <a v-if="task.status === 'done'" class="icon-button" :href="fileUrl(task)" :aria-label="`保存 ${name(task)}`" title="保存文件" download><AppIcon name="arrow" :size="17" /></a>
                  <button v-else-if="['resume', 'pending'].includes(task.status)" type="button" class="icon-button" :disabled="pending.has(task.url)" :aria-label="`暂停 ${name(task)}`" title="暂停" @click="action(task, 'pause')"><AppIcon name="pause" :size="17" /></button>
                  <button v-else type="button" class="icon-button" :disabled="pending.has(task.url)" :aria-label="task.status === 'error' && task.tsFailed && task.hasSegmentDetails ? `仅重试 ${name(task)} 的失败分片` : `继续 ${name(task)}`" :title="task.status === 'error' && task.tsFailed && task.hasSegmentDetails ? '仅重试失败分片' : '继续下载'" @click="action(task, task.status === 'error' && task.tsFailed && task.hasSegmentDetails ? 'retry-failed' : 'resume')"><AppIcon :name="task.status === 'error' ? 'retry' : 'resume'" :size="17" /></button>
                  <button v-if="!task.browserCapture && ['pause', 'error'].includes(task.status)" type="button" class="icon-button" :aria-label="`编辑 ${name(task)} 的请求头`" title="编辑请求头" @click="edit(task)"><AppIcon name="edit" :size="16" /></button>
                  <button type="button" class="icon-button remove-button" :aria-label="`移除 ${name(task)}`" title="移除任务" :disabled="batchBusy || pending.has(task.url)" @click="remove([task])"><AppIcon name="trash" :size="17" /></button>
                </div>
              </div>
            </article>
          </div>

          <footer class="zen-panel-foot">
            <span>共 {{ visible.length }} 个任务<span v-if="running"> · {{ running }} 个正在运行</span></span>
            <span class="total-speed"><AppIcon name="arrow" :size="13" />{{ bytes(speed) }}/s</span>
          </footer>
        </section>
      </template>

      <template v-else-if="view === 'rss'">
        <h1 class="zc-sr-only">RSS 订阅中心</h1>

        <div class="zen-rss-view">
          <div v-if="!rssSources.length" class="zen-panel zen-empty">
            <span class="zen-icon zen-icon-orange zen-empty-icon"><AppIcon name="rss" :size="24" /></span>
            <h3>暂无 RSS 订阅源</h3>
            <p>支持拉取上游完整 RSS（不限 500 条）或公开 HTML 列表页；下载完成后自动绑定详情页精准生成 Emby NFO 与 2:3 竖版海报。</p>
            <button type="button" class="button primary" :disabled="connection !== 'online'" @click="openRssEditor(null)">
              <AppIcon name="plus" :size="16" />
              <span>添加 RSS 订阅源</span>
            </button>
          </div>

          <div v-else class="zen-rss-shell" :class="{ 'mobile-detail': rssDetailOpen }">
            <aside class="zen-panel zen-rss-sidebar" aria-label="订阅源列表">
              <div class="zen-rss-sidehead">
                <div class="zen-rss-sidetitle">
                  <span>订阅源</span>
                  <small>{{ rssSources.length }}</small>
                </div>
                <button type="button" class="button secondary" :disabled="connection !== 'online'" @click="openRssEditor(null)">
                  <AppIcon name="plus" :size="15" />
                  <span>添加</span>
                </button>
              </div>

              <div class="zen-rss-sourcelist">
                <button
                  v-for="src in rssSources"
                  :key="src.id"
                  type="button"
                  class="zen-rss-source"
                  :class="{ 'is-error': !!src.lastError }"
                  :aria-current="activeRssSource?.id === src.id ? 'true' : undefined"
                  @click="selectRssSource(src.id)"
                >
                  <span class="zen-icon zen-small-icon" :class="src.lastError ? 'zen-icon-orange' : 'zen-icon-blue'">
                    <AppIcon name="rss" :size="15" />
                  </span>
                  <span class="zen-rss-source-copy">
                    <strong>{{ src.name }}</strong>
                    <span>{{ rssSourceSubline(src) }}</span>
                  </span>
                  <span v-if="src.unreadCount > 0" class="zen-rss-source-unread">{{ src.unreadCount }}</span>
                </button>
              </div>
            </aside>

            <section v-if="activeRssSource" class="zen-panel zen-rss-main" aria-label="订阅条目区">
              <div class="zen-rss-backbar">
                <button type="button" class="button secondary" @click="rssDetailOpen = false">
                  <AppIcon name="back" :size="16" />
                  <span>返回订阅源</span>
                </button>
              </div>

              <div class="zen-rss-head">
                <div class="zen-rss-titleblock">
                  <div class="zen-rss-titleline">
                    <h2>{{ activeRssSource.name }}</h2>
                    <span class="zen-mode-badge" :class="activeRssSource.mode">{{ activeRssSource.mode === 'html' ? 'HTML' : 'RSS' }}</span>
                    <span v-if="activeRssSource.autoDownload" class="zen-mode-badge auto">自动下载</span>
                    <span v-if="activeRssSource.autoScrape" class="zen-mode-badge">自动 NFO</span>
                  </div>
                  <p class="zen-rss-sub">
                    共 {{ activeRssSource.totalCount || 0 }} 条 · {{ activeRssSource.unreadCount || 0 }} 条新条目 · {{ activeRssSource.url }}
                  </p>
                </div>

                <div class="zen-rss-head-actions">
                  <button
                    type="button"
                    class="button secondary"
                    :disabled="rssRefreshingId === activeRssSource.id || connection !== 'online'"
                    @click="refreshActiveRss"
                  >
                    <span v-if="rssRefreshingId === activeRssSource.id" class="spinner mini" />
                    <AppIcon v-else name="retry" :size="15" />
                    <span>{{ rssRefreshingId === activeRssSource.id ? '刷新中…' : '立即刷新' }}</span>
                  </button>
                  <button
                    type="button"
                    class="button secondary"
                    :disabled="rssActionBusy || !(rssItemsPage?.counts.new || activeRssSource.unreadCount) || connection !== 'online'"
                    @click="markAllRssRead"
                  >
                    <AppIcon name="check" :size="15" />
                    <span>全部标已读</span>
                  </button>
                  <button type="button" class="button secondary" :disabled="connection !== 'online'" @click="openRssEditor(activeRssSource)">
                    <AppIcon name="edit" :size="15" />
                    <span>编辑</span>
                  </button>
                  <button type="button" class="button secondary" :disabled="connection !== 'online'" @click="openDeleteRss(activeRssSource)">
                    <AppIcon name="trash" :size="15" />
                    <span>删除</span>
                  </button>
                </div>
              </div>

              <p v-if="activeRssSource.lastError" class="zen-rss-error-banner" role="alert">
                最近抓取失败：{{ activeRssSource.lastError }}
              </p>

              <div class="zen-rss-controls">
                <div class="zen-segmented zen-rss-tabs" role="tablist" aria-label="条目状态筛选">
                  <button type="button" :class="{ 'is-active': rssStatusFilter === 'all' }" @click="setRssStatusFilter('all')">
                    全部 <span>{{ rssItemsPage?.counts.all ?? activeRssSource.totalCount ?? 0 }}</span>
                  </button>
                  <button type="button" :class="{ 'is-active': rssStatusFilter === 'new' }" @click="setRssStatusFilter('new')">
                    新条目 <span>{{ rssItemsPage?.counts.new ?? activeRssSource.unreadCount ?? 0 }}</span>
                  </button>
                  <button type="button" :class="{ 'is-active': rssStatusFilter === 'downloaded' }" @click="setRssStatusFilter('downloaded')">
                    已下载 <span>{{ rssItemsPage?.counts.downloaded ?? 0 }}</span>
                  </button>
                  <button type="button" :class="{ 'is-active': rssStatusFilter === 'ignored' }" @click="setRssStatusFilter('ignored')">
                    已忽略 <span>{{ rssItemsPage?.counts.ignored ?? 0 }}</span>
                  </button>
                  <button type="button" :class="{ 'is-active': rssStatusFilter === 'filtered' }" @click="setRssStatusFilter('filtered')">
                    已过滤 <span>{{ (rssItemsPage?.counts.filtered ?? 0) + (rssItemsPage?.counts.duplicateEpisode ?? 0) }}</span>
                  </button>
                  <button type="button" :class="{ 'is-active': rssStatusFilter === 'seed-skipped' }" @click="setRssStatusFilter('seed-skipped')">
                    历史跳过 <span>{{ rssItemsPage?.counts.seedSkipped ?? 0 }}</span>
                  </button>
                </div>

                <div class="zen-rss-search">
                  <label class="zen-search">
                    <AppIcon name="search" :size="16" />
                    <input v-model="rssSearch" type="search" placeholder="搜索全量标题或标签" aria-label="搜索 RSS 条目" />
                  </label>
                  <button type="button" class="button secondary" @click="toggleRssSort">
                    <span>{{ rssSort === 'newest' ? '最新在前' : '最旧在前' }}</span>
                  </button>
                </div>
              </div>

              <fieldset v-if="rssItemsPage?.items.length" class="zen-toolbar" :class="{ 'has-selection': rssSelectedGuids.size }">
                <legend class="sr-only">批量操作订阅条目</legend>
                <label class="task-checkbox select-all">
                  <input
                    type="checkbox"
                    :checked="allRssSelected"
                    :disabled="rssActionBusy || !(rssItemsPage?.items.length)"
                    aria-label="全选本页条目"
                    @change="toggleAllRssItems"
                  />
                  <span>{{ rssSelectedGuids.size ? `已选 ${rssSelectedGuids.size} 项` : '全选本页' }}</span>
                </label>

                <div v-if="rssSelectedGuids.size" class="selection-actions">
                  <button
                    type="button"
                    class="button primary"
                    :disabled="rssActionBusy || connection !== 'online'"
                    @click="actRssItems([...rssSelectedGuids], 'download')"
                  >
                    <AppIcon name="arrow" :size="15" />
                    <span>下载所选 ({{ rssSelectedGuids.size }})</span>
                  </button>
                  <button
                    type="button"
                    class="button secondary"
                    :disabled="rssActionBusy || connection !== 'online'"
                    @click="actRssItems([...rssSelectedGuids], 'ignore')"
                  >
                    <AppIcon name="eyeOff" :size="15" />
                    <span>忽略所选</span>
                  </button>
                  <button type="button" class="toolbar-button clear-selection" :disabled="rssActionBusy" @click="rssSelectedGuids.clear()">
                    <AppIcon name="close" :size="14" />
                    <span>取消选择</span>
                  </button>
                </div>
                <span v-else class="selection-hint">勾选条目可批量下载或忽略</span>
              </fieldset>

              <p v-if="rssItemsError" class="zen-rss-error-banner" role="alert">{{ rssItemsError }}</p>

              <div class="zen-rss-items">
                <div v-if="rssItemsLoading && !rssItemsPage?.items.length" class="loading-state" role="status">
                  <div v-for="n in 3" :key="n" class="zen-skeleton-card"><span /><span /><span /></div>
                  <p>正在加载订阅条目…</p>
                </div>

                <div v-else-if="!rssItemsPage?.items.length" class="zen-empty zen-empty-compact">
                  <span class="zen-icon zen-icon-orange zen-empty-icon"><AppIcon name="rss" :size="22" /></span>
                  <h3>{{ rssSearch ? '没有匹配的订阅条目' : '暂无订阅条目' }}</h3>
                  <p>{{ rssSearch ? '可以换个关键词，或切换上方状态筛选标签后再试。' : '点击右上角「立即刷新」从上游拉取完整列表。' }}</p>
                  <button v-if="rssSearch" type="button" class="button secondary" @click="rssSearch = ''">清除搜索</button>
                  <button v-else type="button" class="button primary" :disabled="rssRefreshingId === activeRssSource.id" @click="refreshActiveRss">
                    <AppIcon name="retry" :size="15" />
                    <span>立即刷新</span>
                  </button>
                </div>

                <article
                  v-for="item in rssItemsPage.items"
                  v-else
                  :key="item.guid"
                  class="zen-rss-item"
                  :class="{ 'row-selected': rssSelectedGuids.has(item.guid) }"
                >
                  <div class="zen-rss-item-top">
                    <label class="task-checkbox">
                      <input
                        type="checkbox"
                        :checked="rssSelectedGuids.has(item.guid)"
                        :aria-label="`选择 ${item.title}`"
                        @change="toggleRssGuid(item.guid)"
                      />
                      <span class="sr-only">选择 {{ item.title }}</span>
                    </label>
                    <div class="zen-rss-item-body">
                      <strong class="zen-rss-item-title">{{ item.title }}</strong>
                      <div class="zen-rss-item-meta">
                        <span v-if="item.pubDate">{{ formatTime(item.pubDate) }}</span>
                        <span v-else-if="item.pubDateText">{{ item.pubDateText }}</span>
                        <span v-if="item.episodeKey">剧集 {{ item.episodeKey }}</span>
                        <span v-if="item.enclosureLength > 0">{{ bytes(item.enclosureLength) }}</span>
                        <span>{{ host(item.link) }}</span>
                      </div>
                      <div v-if="item.tags.length" class="zen-rss-item-tags">
                        <span v-for="tag in item.tags.slice(0, 5)" :key="tag" class="zen-rss-tag">{{ tag }}</span>
                      </div>
                    </div>
                  </div>

                  <div class="zen-rss-item-foot">
                    <div class="zen-rss-item-status">
                      <span class="zen-status" :class="rssItemStatusLabel(item).tone">{{ rssItemStatusLabel(item).text }}</span>
                      <span v-if="rssReasonLabel(item)" class="zen-rss-reason">{{ rssReasonLabel(item) }}</span>
                    </div>

                    <div class="zen-rss-item-actions">
                      <button
                        type="button"
                        class="button secondary"
                        :disabled="rssActionBusy || connection !== 'online'"
                        @click="actRssItems([item.guid], 'download')"
                      >
                        <AppIcon name="arrow" :size="14" />
                        <span>{{ item.status === 'downloaded' ? '再次下载' : '下载' }}</span>
                      </button>
                      <button
                        v-if="item.status === 'new' || item.status === 'filtered' || item.status === 'duplicate-episode' || item.status === 'seed-skipped'"
                        type="button"
                        class="button secondary"
                        :disabled="rssActionBusy || connection !== 'online'"
                        @click="actRssItems([item.guid], 'ignore')"
                      >
                        <AppIcon name="eyeOff" :size="14" />
                        <span>忽略</span>
                      </button>
                      <button
                        v-else-if="item.status === 'ignored'"
                        type="button"
                        class="button secondary"
                        :disabled="rssActionBusy || connection !== 'online'"
                        @click="actRssItems([item.guid], 'mark-new')"
                      >
                        <span>标为新条目</span>
                      </button>
                    </div>
                  </div>
                </article>
              </div>

              <div v-if="rssItemsPage" class="zen-rss-pagination">
                <span>第 {{ rssItemsPage.page }} / {{ rssTotalPages }} 页 · 筛选 {{ rssItemsPage.total }} 条 / 共 {{ rssItemsPage.counts.all }} 条</span>
                <div class="zen-rss-pagecontrols">
                  <select v-model.number="rssPageSize" aria-label="每页显示条数" @change="onRssPageSizeChange">
                    <option :value="50">50 条/页</option>
                    <option :value="100">100 条/页</option>
                    <option :value="200">200 条/页</option>
                  </select>
                  <button type="button" class="button secondary" :disabled="rssItemsPage.page <= 1 || rssItemsLoading" @click="changeRssPage(rssItemsPage.page - 1)">上一页</button>
                  <button type="button" class="button secondary" :disabled="rssItemsPage.page >= rssTotalPages || rssItemsLoading" @click="changeRssPage(rssItemsPage.page + 1)">下一页</button>
                </div>
              </div>
            </section>
          </div>
        </div>
      </template>

      <template v-else-if="view === 'scrape'">
        <h1 class="zc-sr-only">媒体刮削</h1>

        <div class="zen-settings-stack zen-scrape-view">
          <div class="zen-settings-grid">
            <section class="zen-panel zen-settings-card">
              <div class="zen-card-head">
                <span class="zen-icon zen-section-icon zen-icon-orange"><AppIcon name="film" :size="18" /></span>
                <div>
                  <h2>本地扫描与元数据数据源</h2>
                  <p>支持多目录扫描，并按优先级依次检索 RSS 全量索引与 WordPress 站点</p>
                </div>
              </div>

              <div class="zen-label-row">
                <label for="scrape-scan-dir-0">扫描目录（支持多个）</label>
                <div class="zen-label-actions">
                  <span v-if="scrapeConfigSavedAt" class="zen-saved-hint">已自动保存</span>
                  <button
                    type="button"
                    class="button secondary zen-btn-xs"
                    :disabled="scrapeScanDirs.length >= 20 || scrapeScanBusy || scrapeRunBusy"
                    @click="addScrapeScanDir"
                  >
                    + 添加目录
                  </button>
                </div>
              </div>
              <div class="zen-dir-list">
                <div v-for="(_dir, idx) in scrapeScanDirs" :key="idx" class="zen-dir-row">
                  <input
                    :id="`scrape-scan-dir-${idx}`"
                    v-model="scrapeScanDirs[idx]"
                    placeholder="/media/tg"
                    :aria-label="`扫描目录 ${idx + 1}`"
                  />
                  <button
                    v-if="scrapeScanDirs.length > 1"
                    type="button"
                    class="icon-button"
                    :disabled="scrapeScanBusy || scrapeRunBusy"
                    aria-label="删除该扫描目录"
                    @click="removeScrapeScanDir(idx)"
                  >
                    <AppIcon name="close" :size="16" />
                  </button>
                </div>
              </div>
              <p class="field-help">支持添加多个容器内挂载目录（自动去重）；所有目录与数据源配置修改后自动保存至服务端。</p>

              <div class="zen-label-row">
                <label for="scrape-source-url-0">元数据数据源（按优先级从上到下搜索）</label>
                <div class="zen-label-actions">
                  <button
                    type="button"
                    class="button secondary zen-btn-xs"
                    :disabled="scrapeSources.length >= 20 || scrapeScanBusy || scrapeRunBusy"
                    @click="addScrapeSource('rss')"
                  >
                    + RSS 源
                  </button>
                  <button
                    type="button"
                    class="button secondary zen-btn-xs"
                    :disabled="scrapeSources.length >= 20 || scrapeScanBusy || scrapeRunBusy"
                    @click="addScrapeSource('wordpress')"
                  >
                    + WordPress 源
                  </button>
                </div>
              </div>
              <div v-if="rssSources.some(item => item.mode === 'rss')" class="zen-source-import-row">
                <select v-model="scrapeImportFeedId" aria-label="从已有订阅源导入" @change="importRssSourceToScrape">
                  <option value="">从已有 RSS 订阅源快速添加到列表…</option>
                  <option v-for="feed in rssSources.filter(item => item.mode === 'rss')" :key="feed.id" :value="feed.id">
                    {{ feed.name || feed.id }}（{{ feed.url }}）
                  </option>
                </select>
              </div>
              <div class="zen-source-list">
                <div
                  v-for="(src, idx) in scrapeSources"
                  :key="src.id"
                  class="zen-source-item"
                  :class="{ 'is-disabled': !src.enabled }"
                >
                  <div class="zen-source-top">
                    <span class="zen-source-rank">#{{ idx + 1 }}</span>
                    <label class="zen-source-toggle">
                      <input v-model="src.enabled" type="checkbox" :aria-label="`启用数据源 ${idx + 1}`" />
                      <span>{{ src.enabled ? '启用' : '停用' }}</span>
                    </label>
                    <select v-model="src.type" class="zen-source-type" :aria-label="`数据源 ${idx + 1} 类型`">
                      <option value="rss">RSS 全量索引</option>
                      <option value="wordpress">WordPress 搜索</option>
                    </select>
                    <input
                      v-model="src.name"
                      class="zen-source-name"
                      placeholder="数据源名称"
                      :aria-label="`数据源 ${idx + 1} 名称`"
                    />
                    <div class="zen-source-controls">
                      <button
                        type="button"
                        class="icon-button"
                        :disabled="idx === 0 || scrapeScanBusy || scrapeRunBusy"
                        aria-label="上移优先级"
                        title="上移优先级"
                        @click="moveScrapeSource(idx, -1)"
                      >
                        <AppIcon name="arrowUp" :size="15" />
                      </button>
                      <button
                        type="button"
                        class="icon-button"
                        :disabled="idx === scrapeSources.length - 1 || scrapeScanBusy || scrapeRunBusy"
                        aria-label="下移优先级"
                        title="下移优先级"
                        @click="moveScrapeSource(idx, 1)"
                      >
                        <AppIcon name="arrowDown" :size="15" />
                      </button>
                      <button
                        v-if="scrapeSources.length > 1"
                        type="button"
                        class="icon-button"
                        :disabled="scrapeScanBusy || scrapeRunBusy"
                        aria-label="删除该数据源"
                        title="删除"
                        @click="removeScrapeSource(idx)"
                      >
                        <AppIcon name="close" :size="15" />
                      </button>
                    </div>
                  </div>
                  <div class="zen-source-fields">
                    <input
                      :id="`scrape-source-url-${idx}`"
                      v-model="src.url"
                      type="url"
                      :placeholder="src.type === 'wordpress' ? 'https://example.com（填写站点域名即可）' : 'https://example.com/rss.xml'"
                      :aria-label="`数据源 ${idx + 1} 地址`"
                    />
                    <input
                      v-model="src.referer"
                      type="url"
                      placeholder="Referer（可选，留空自动）"
                      :aria-label="`数据源 ${idx + 1} Referer`"
                    />
                  </div>
                </div>
              </div>
              <p class="field-help">优先使用排名靠前的源；高置信命中后自动跳过后续 WordPress 站点检索，避免无效请求。点击 ↑ / ↓ 可调整顺序。</p>

              <label for="scrape-min-score">高置信阈值 (0–100)</label>
              <input id="scrape-min-score" v-model.number="scrapeMinScore" type="number" min="0" max="100" />

              <label class="check-field zen-nested-check">
                <input v-model="scrapeRecursive" type="checkbox" />
                <span>递归扫描子目录<small>自动跳过 @eaDir、#recycle 与隐藏临时文件。</small></span>
              </label>

              <label class="check-field zen-nested-check">
                <input v-model="scrapeSkipExistingNfo" type="checkbox" />
                <span>跳过已有 .nfo 的视频<small>避免重复刮削已整理完的媒体；重刮可取消勾选。</small></span>
              </label>
            </section>

            <section class="zen-panel zen-settings-card">
              <div class="zen-card-head">
                <span class="zen-icon zen-section-icon zen-icon-purple"><AppIcon name="folder" :size="18" /></span>
                <div>
                  <h2>Emby 刮削与整理策略</h2>
                  <p>生成标准 Emby / Jellyfin NFO，支持保留原横图与 FFmpeg 2:3 竖版海报裁切</p>
                </div>
              </div>

              <label for="scrape-mode">整理方式</label>
              <select id="scrape-mode" v-model="scrapeMode">
                <option value="in-place">原地刮削（同名 .nfo + 封面）</option>
                <option value="organize">按目录归档（独立影片目录）</option>
              </select>

              <template v-if="scrapeMode === 'organize'">
                <label for="scrape-output-dir">归档输出目录</label>
                <input id="scrape-output-dir" v-model="scrapeOutputDir" placeholder="/app/downloads/emby" />

                <div class="form-grid">
                  <div>
                    <label for="scrape-transfer">文件处理方式</label>
                    <select id="scrape-transfer" v-model="scrapeTransferMode">
                      <option value="move">移动文件（推荐）</option>
                      <option value="hardlink">硬链接（同分区零拷贝）</option>
                      <option value="copy">复制文件</option>
                    </select>
                  </div>
                  <div>
                    <label for="scrape-folder">目录层级结构</label>
                    <select id="scrape-folder" v-model="scrapeFolderTemplate">
                      <option value="actor-title">演员 / 日期+片名 / 视频</option>
                      <option value="title">日期+片名 / 视频</option>
                    </select>
                  </div>
                </div>
              </template>

              <div class="form-grid">
                <div>
                  <label for="scrape-crop">封面海报裁切（2:3 竖版 poster）</label>
                  <select id="scrape-crop" v-model="scrapePosterCrop">
                    <option value="center">2:3 居中裁切海报（保留原横图）</option>
                    <option value="right">2:3 靠右裁切海报（保留原横图）</option>
                    <option value="none">不裁切（原横图共用）</option>
                  </select>
                </div>
                <div>
                  <label for="scrape-concurrency">刮削并发数</label>
                  <select id="scrape-concurrency" v-model.number="scrapeConcurrency">
                    <option :value="1">1 并发（低负载）</option>
                    <option :value="3">3 并发（默认）</option>
                    <option :value="5">5 并发（较快）</option>
                    <option :value="8">8 并发（高速）</option>
                    <option :value="10">10 并发（极限）</option>
                  </select>
                </div>
              </div>
              <p class="field-help">原横图自动保存为 fanart/thumb，竖版 poster 由 FFmpeg 按 2:3 裁切；网络允许时提高并发可显著加速。</p>

              <label class="check-field zen-nested-check">
                <input v-model="scrapeDownloadImages" type="checkbox" />
                <span>下载封面图并生成 poster / fanart / thumb<small>自动携带上游 Referer 拉取封面图，并按上面策略处理海报。</small></span>
              </label>

              <label class="check-field zen-nested-check">
                <input v-model="scrapeOverwriteNfo" type="checkbox" />
                <span>覆盖已存在的 .nfo 与封面图<small>默认仅在不存在时写入；如需刷新元数据与封面请勾选。</small></span>
              </label>

              <div class="zen-scrape-actions">
                <button type="button" class="button primary" :disabled="scrapeScanBusy || scrapeRunBusy || connection !== 'online'" @click="scanScrape(false)">
                  <span v-if="scrapeScanBusy" class="spinner" />
                  <AppIcon v-else name="search" :size="16" />
                  <span>{{ scrapeScanBusy ? '扫描匹配中…' : '扫描并预览匹配' }}</span>
                </button>
                <button type="button" class="button secondary" :disabled="scrapeScanBusy || scrapeRunBusy || connection !== 'online'" @click="scanScrape(true)">
                  <AppIcon name="retry" :size="15" />
                  <span>刷新源缓存后匹配</span>
                </button>
              </div>
              <p v-if="scrapeError" class="form-error" role="alert">{{ scrapeError }}</p>
            </section>
          </div>

          <section v-if="scrapeJob" class="zen-panel zen-settings-card" aria-label="后台刮削状态">
            <div class="zen-card-head zen-card-head-split">
              <div class="zen-card-head-title">
                <span
                  class="zen-icon zen-section-icon"
                  :class="scrapeJob.status === 'running' ? 'zen-icon-blue' : scrapeJob.status === 'done' ? 'zen-icon-green' : 'zen-icon-orange'"
                >
                  <AppIcon :name="scrapeJob.status === 'running' ? 'activity' : scrapeJob.status === 'done' ? 'check' : 'alert'" :size="18" />
                </span>
                <div>
                  <h2 v-if="scrapeJob.status === 'running'">
                    后台刮削运行中：已完成 {{ scrapeJob.done }} / {{ scrapeJob.total }}（成功 {{ scrapeJob.succeeded }}，失败 {{ scrapeJob.failed }}）
                  </h2>
                  <h2 v-else-if="scrapeJob.status === 'done'">
                    后台刮削完成：成功 {{ scrapeJob.succeeded }} / {{ scrapeJob.total }}<template v-if="scrapeJob.failed">（失败 {{ scrapeJob.failed }}）</template>
                  </h2>
                  <h2 v-else-if="scrapeJob.status === 'stopped'">
                    后台刮削已暂停：已完成 {{ scrapeJob.done }} / {{ scrapeJob.total }}（成功 {{ scrapeJob.succeeded }}，失败 {{ scrapeJob.failed }}）
                  </h2>
                  <h2 v-else>
                    后台刮削异常：已处理 {{ scrapeJob.done }} / {{ scrapeJob.total }}（成功 {{ scrapeJob.succeeded }}，失败 {{ scrapeJob.failed }}）
                  </h2>
                  <p v-if="scrapeJob.status === 'running'">
                    正在处理：{{ scrapeJob.currentFile || '准备中…' }} · {{ scrapeJob.concurrency }} 并发<template v-if="scrapeSpeedText"> · {{ scrapeSpeedText }}</template> · 关闭浏览器将继续在服务端运行
                  </p>
                  <p v-else-if="scrapeJob.status === 'done'">在 Emby 媒体库中执行「刷新元数据 / 扫描媒体库文件」即可识别生成的 .nfo 与海报</p>
                  <p v-else-if="scrapeJob.status === 'stopped'">剩余 {{ scrapeJob.remaining }} 个视频可直接点「继续剩余」接续刮削，无需重新扫描（容器重启也会保留进度）</p>
                  <p v-else>{{ scrapeJob.error || '刮削任务中断，可直接点「继续剩余」或查看服务端日志定位原因' }}</p>
                </div>
              </div>
              <div class="zen-scrape-job-actions">
                <button
                  v-if="scrapeJob.status === 'running'"
                  type="button"
                  class="button danger"
                  :disabled="scrapeStopBusy || connection !== 'online'"
                  @click="stopScrape"
                >
                  <span v-if="scrapeStopBusy" class="spinner" />
                  <span>{{ scrapeStopBusy ? '正在暂停…' : '暂停刮削' }}</span>
                </button>
                <template v-else>
                  <button
                    v-if="scrapeJob.canResume"
                    type="button"
                    class="button primary"
                    :disabled="scrapeResumeBusy || scrapeScanBusy || connection !== 'online'"
                    @click="resumeScrape(false)"
                  >
                    <span v-if="scrapeResumeBusy" class="spinner" />
                    <AppIcon v-else name="resume" :size="15" />
                    <span>继续剩余 ({{ scrapeJob.remaining }})</span>
                  </button>
                  <button
                    v-if="scrapeJob.canRetryFailed"
                    type="button"
                    class="button secondary"
                    :disabled="scrapeResumeBusy || scrapeScanBusy || connection !== 'online'"
                    @click="resumeScrape(true)"
                  >
                    <AppIcon name="retry" :size="15" />
                    <span>重试失败 ({{ scrapeJob.failed }})</span>
                  </button>
                  <button
                    type="button"
                    class="button secondary"
                    :disabled="scrapeResumeBusy || connection !== 'online'"
                    @click="clearScrapeJob"
                  >
                    清空记录
                  </button>
                </template>
              </div>
            </div>
            <div v-if="scrapeJob.total > 0" class="zen-scrape-progress" role="progressbar" :aria-valuenow="scrapeJob.done" aria-valuemin="0" :aria-valuemax="scrapeJob.total">
              <div class="zen-scrape-progress-bar" :style="{ width: `${scrapePercent}%` }" />
            </div>
            <div v-if="scrapeTimingSummary" class="zen-scrape-diag">
              <div class="zen-scrape-diag-info">
                <span>平均耗时：总 {{ scrapeTimingSummary.avgTotal }}ms（详情 {{ scrapeTimingSummary.avgDetail }}ms · 文件 {{ scrapeTimingSummary.avgTransfer }}ms · 封面 {{ scrapeTimingSummary.avgArt }}ms · 裁图 {{ scrapeTimingSummary.avgCrop }}ms）</span>
                <span :class="{ 'zen-scrape-diag-warn': scrapeTimingSummary.warn }">{{ scrapeTimingSummary.hint }}</span>
              </div>
              <button type="button" class="button secondary" @click="copyScrapeLogs">复制诊断日志</button>
            </div>
            <p v-if="scrapeJob.totalResults > visibleScrapeOutcomes.length" class="zen-scrape-outcome-note">
              共 {{ scrapeJob.totalResults }} 条记录，为保持页面流畅，当前展示最近失败项及最近成功项（最多 {{ visibleScrapeOutcomes.length }} 条）。
            </p>
            <ul v-if="visibleScrapeOutcomes.length" class="zen-scrape-outcome">
              <li v-for="item in visibleScrapeOutcomes" :key="item.filePath" :class="item.status">
                <strong>{{ item.title || host(item.filePath) }}</strong>
                <span v-if="item.status === 'success'" class="outcome-path">{{ item.nfoPath }}<template v-if="item.posterPath"> · 含海报</template></span>
                <span v-else class="outcome-err">{{ item.message }}</span>
                <span v-if="item.timing" class="outcome-timing">{{ formatTimingLine(item) }}</span>
              </li>
            </ul>
          </section>

          <section v-if="scrapeScanResult" class="zen-panel">
            <div class="zen-panel-head">
              <div class="zen-panel-title">
                <span class="zen-icon zen-section-icon zen-icon-blue"><AppIcon name="layers" :size="18" /></span>
                <div>
                  <h2>匹配预览 · {{ scrapeScanResult.feedTitle }}</h2>
                  <p><template v-if="scrapeScanResult.scanDirs && scrapeScanResult.scanDirs.length > 1">已扫描 {{ scrapeScanResult.scanDirs.length }} 个目录 · </template><template v-if="scrapeScanResult.catalogCount > 0">索引 {{ scrapeScanResult.catalogCount }} 条 · </template>本地发现 {{ scrapeScanResult.scannedFiles }} 个视频 · 高置信匹配 {{ scrapeScanResult.matchedCount }} 个</p>
                </div>
              </div>

              <div class="zen-panel-controls">
                <div class="zen-segmented" role="tablist" aria-label="刮削筛选">
                  <button type="button" :class="{ 'is-active': scrapeFilter === 'all' }" @click="scrapeFilter = 'all'">全部 <span>{{ scrapeScanResult.items.length }}</span></button>
                  <button type="button" :class="{ 'is-active': scrapeFilter === 'matched' }" @click="scrapeFilter = 'matched'">高置信 <span>{{ scrapeScanResult.items.filter(i => i.status === 'matched').length }}</span></button>
                  <button type="button" :class="{ 'is-active': scrapeFilter === 'low' }" @click="scrapeFilter = 'low'">待确认 <span>{{ scrapeScanResult.items.filter(i => i.status === 'low-confidence').length }}</span></button>
                  <button type="button" :class="{ 'is-active': scrapeFilter === 'unmatched' }" @click="scrapeFilter = 'unmatched'">其他 <span>{{ scrapeScanResult.items.filter(i => i.status === 'unmatched' || i.status === 'skipped-nfo').length }}</span></button>
                </div>
              </div>
            </div>

            <div v-if="scrapeScanResult.sourceSummaries?.length" class="zen-source-summary-row">
              <span
                v-for="(sum, sIdx) in scrapeScanResult.sourceSummaries"
                :key="`${sum.id}-${sIdx}`"
                class="zen-source-chip"
                :class="{ 'is-error': Boolean(sum.error) }"
              >
                <strong>#{{ sIdx + 1 }} {{ sum.name }}</strong>
                <template v-if="sum.error">请求失败：{{ sum.error }}</template>
                <template v-else-if="sum.type === 'rss'">索引 {{ sum.catalogCount }} 条 · 命中 {{ sum.matchedCount }}</template>
                <template v-else>兜底检索 {{ sum.queriedCount }} 个 · 命中 {{ sum.matchedCount }}</template>
              </span>
            </div>

            <fieldset class="zen-toolbar" :class="{ 'has-selection': scrapeSelected.size }">
              <legend class="sr-only">批量刮削操作</legend>
              <label class="task-checkbox select-all">
                <input type="checkbox" :checked="allScrapeSelected" :disabled="!selectableScrapeItems.length || scrapeRunBusy" aria-label="全选当前筛选结果" @change="toggleAllScrape" />
                <span>{{ scrapeSelected.size ? `已选 ${scrapeSelected.size} 项` : '全选可刮削项' }}</span>
              </label>

              <div class="batch-actions">
                <button type="button" class="button primary" :disabled="!scrapeSelected.size || scrapeRunBusy || scrapeScanBusy || connection !== 'online'" @click="executeScrape">
                  <span v-if="scrapeRunBusy" class="spinner" />
                  <AppIcon v-else name="check" :size="15" />
                  <span>{{ scrapeRunBusy ? (scrapeJob ? `后台刮削中 ${scrapeJob.done}/${scrapeJob.total}…` : '正在启动后台刮削…') : `开始刮削所选 (${scrapeSelected.size})` }}</span>
                </button>
                <button v-if="scrapeJob?.status === 'running'" type="button" class="button secondary" :disabled="scrapeStopBusy" @click="stopScrape">
                  {{ scrapeStopBusy ? '正在暂停…' : '暂停' }}
                </button>
              </div>
            </fieldset>

            <div class="zen-scrape-list">
              <div v-if="!visibleScrapeItems.length" class="zen-empty">
                <span class="zen-icon zen-icon-orange zen-empty-icon"><AppIcon name="search" :size="24" /></span>
                <h3>当前筛选下没有视频</h3>
                <p>可以切换筛选标签，或调整扫描目录与匹配分值阈值后再试。</p>
              </div>

              <article v-for="item in visibleScrapeItems" v-else :key="item.filePath" class="zen-scrape-card" :class="{ 'row-selected': scrapeSelected.has(item.filePath) }">
                <div class="zen-scrape-head">
                  <label class="task-checkbox">
                    <input type="checkbox" :checked="scrapeSelected.has(item.filePath)" :disabled="!scrapeChoice[item.filePath] || scrapeRunBusy" :aria-label="`选择 ${item.filename}`" @change="toggleScrapeItem(item.filePath)" />
                    <span class="sr-only">选择 {{ item.filename }}</span>
                  </label>
                  <div class="zen-scrape-file">
                    <strong>{{ item.filename }}</strong>
                    <span>
                      {{ item.relativePath }} · {{ bytes(item.size) }}
                      <template v-if="item.hasNfo"> · 已有 .nfo</template>
                      <template v-if="item.match?.sourceName && (scrapeScanResult?.sourceSummaries?.length || 0) > 1"> · 来源：{{ item.match.sourceName }}</template>
                    </span>
                  </div>
                  <span
                    class="zen-score-pill"
                    :class="item.status === 'skipped-nfo' ? 'skipped' : item.status === 'matched' ? 'high' : item.status === 'low-confidence' ? 'mid' : 'none'"
                  >
                    {{ item.status === 'skipped-nfo' ? '已有 NFO 跳过' : item.match ? `匹配 ${item.match.score}%` : '未匹配' }}
                  </span>
                </div>

                <div class="zen-scrape-match-row">
                  <select v-model="scrapeChoice[item.filePath]" :aria-label="`为 ${item.filename} 选择匹配条目`" @change="onScrapeChoiceChange(item)">
                    <option value="">不刮削此文件</option>
                    <option v-for="cand in item.candidates" :key="`${cand.sourceId || ''}-${cand.guid}`" :value="cand.pageUrl">
                      [{{ cand.score }}%]{{ cand.sourceName && (scrapeScanResult?.sourceSummaries?.length || 0) > 1 ? ` [${cand.sourceName}]` : '' }} {{ cand.title }}
                    </option>
                  </select>
                  <a v-if="scrapeChoice[item.filePath]" class="button secondary" :href="scrapeChoice[item.filePath]" target="_blank" rel="noopener noreferrer">来源页</a>
                </div>
              </article>
            </div>
          </section>
        </div>
      </template>

      <template v-else>
        <h1 class="zc-sr-only">系统设置</h1>

        <div class="zen-settings-bar">
          <div class="zen-segmented zen-settings-tabs" role="tablist" aria-label="设置分类" @keydown="navigateSettingsTabs">
            <button id="settings-download-tab" type="button" role="tab" :tabindex="settingsTab === 'download' ? 0 : -1" :aria-selected="settingsTab === 'download'" :class="{ 'is-active': settingsTab === 'download' }" @click="settingsTab = 'download'">下载与合并</button>
            <button id="settings-network-tab" type="button" role="tab" :tabindex="settingsTab === 'network' ? 0 : -1" :aria-selected="settingsTab === 'network'" :class="{ 'is-active': settingsTab === 'network' }" @click="settingsTab = 'network'">网络与脚本</button>
            <button id="settings-appearance-tab" type="button" role="tab" :tabindex="settingsTab === 'appearance' ? 0 : -1" :aria-selected="settingsTab === 'appearance'" :class="{ 'is-active': settingsTab === 'appearance' }" @click="settingsTab = 'appearance'">外观与主题</button>
          </div>
        </div>

        <form class="zen-settings-form" @submit.prevent="saveSettings">
          <div v-if="settingsTab === 'download'" class="zen-settings-grid">
            <section class="zen-panel zen-settings-card">
              <div class="zen-card-head">
                <span class="zen-icon zen-section-icon zen-icon-blue"><AppIcon name="settings" :size="18" /></span>
                <div>
                  <h2>保存与并发</h2>
                  <p>控制服务端落盘目录与下载请求默认凭据</p>
                </div>
              </div>
              <label for="save-dir">文件保存目录</label>
              <input id="save-dir" v-model="config.saveDir" required />
              <p class="field-help">运行下载器的服务端目录路径。</p>

              <div class="form-grid">
                <div>
                  <label for="threads">每任务并发分片</label>
                  <input id="threads" v-model.number="config.threadNum" type="number" min="1" max="16" required />
                </div>
                <div>
                  <label for="parallel">同时下载任务</label>
                  <input id="parallel" v-model.number="config.maxDownloads" type="number" min="1" max="5" required />
                </div>
              </div>

              <label for="default-headers">默认请求头</label>
              <textarea id="default-headers" v-model="config.headers" rows="3" class="mono" placeholder="Referer: https://example.com/&#10;Cookie: ..." spellcheck="false" />
              <p class="field-help">所有新任务共享；单个任务填写的同名请求头优先。</p>
            </section>

            <section class="zen-panel zen-settings-card">
              <div class="zen-card-head">
                <span class="zen-icon zen-section-icon zen-icon-teal"><AppIcon name="folder" :size="18" /></span>
                <div>
                  <h2>合并与缓存</h2>
                  <p>配置 FFmpeg 转封装与分片缓存保留策略</p>
                </div>
              </div>
              <label for="ffmpeg">FFmpeg 路径 <span class="field-note">留空使用系统 PATH</span></label>
              <input id="ffmpeg" v-model="config.ffmpegPath" placeholder="ffmpeg" />

              <label for="merge-timeout">合并无进展超时（秒）</label>
              <input id="merge-timeout" v-model.number="config.mergeTimeout" type="number" min="15" max="3600" required />

              <label class="check-field zen-nested-check">
                <input v-model="config.delCache" type="checkbox" />
                <span>合并成功后清理分片缓存<small>失败和暂停时始终保留，方便断点续传。</small></span>
              </label>
            </section>
          </div>

          <div v-else-if="settingsTab === 'network'" class="zen-settings-stack">
            <section class="zen-panel zen-settings-card">
              <div class="zen-card-head">
                <span class="zen-icon zen-section-icon zen-icon-purple"><AppIcon name="link" :size="18" /></span>
                <div>
                  <h2>网络代理与配套工具</h2>
                  <p>配置出站 HTTP 代理或安装浏览器发送脚本</p>
                </div>
              </div>

              <div class="zen-network-row">
                <div>
                  <label for="proxy-mode">网络代理</label>
                  <select id="proxy-mode" v-model="config.proxyMode">
                    <option value="system">使用系统环境变量</option>
                    <option value="disabled">不使用代理</option>
                    <option value="custom">自定义 HTTP 代理</option>
                  </select>

                  <template v-if="config.proxyMode === 'custom'">
                    <label for="proxy-url">代理地址</label>
                    <input id="proxy-url" v-model="config.proxyUrl" type="url" placeholder="http://127.0.0.1:7890" required />
                  </template>
                </div>

                <div class="zen-nested-box">
                  <div class="zen-nested-copy">
                    <strong>脚本猫播放页发送</strong>
                    <p>安装脚本后可在播放页嗅探媒体和字幕并直接发送到本下载器。</p>
                  </div>
                  <a class="button secondary" href="/scriptcat.user.js" target="_blank" rel="noopener noreferrer">
                    <AppIcon name="script" :size="16" />
                    <span>安装脚本猫脚本</span>
                  </a>
                </div>
              </div>
            </section>
          </div>

          <div v-else class="zen-settings-grid">
            <section class="zen-panel zen-settings-card">
              <div class="zen-card-head">
                <span class="zen-icon zen-section-icon zen-icon-orange"><AppIcon name="palette" :size="18" /></span>
                <div>
                  <h2>界面皮肤</h2>
                  <p>在两套正式视觉语言之间即时切换</p>
                </div>
              </div>

              <div class="zen-choice-grid two">
                <button type="button" class="zen-choice-card" :aria-pressed="skin === 'apple'" @click="setSkin('apple')">
                  <div class="zen-choice-head">
                    <strong>Apple 清爽卡片</strong>
                    <span class="zen-swatch"><i style="background:#edf0f5" /><i style="background:#007aff" /></span>
                  </div>
                  <p>纯白实心卡片、柔和蓝主色与大圆角，克制无毛玻璃干扰。</p>
                </button>

                <button type="button" class="zen-choice-card" :aria-pressed="skin === 'meridian'" @click="setSkin('meridian')">
                  <div class="zen-choice-head">
                    <strong>Meridian 橙彩玻璃</strong>
                    <span class="zen-swatch"><i style="background:#f4f4f6" /><i style="background:#ff6a00" /></span>
                  </div>
                  <p>悬浮毛玻璃顶栏、嵌套底色层级与高对比暖橙交互点缀。</p>
                </button>
              </div>
            </section>

            <section class="zen-panel zen-settings-card">
              <div class="zen-card-head">
                <span class="zen-icon zen-section-icon zen-icon-blue"><AppIcon name="sun" :size="18" /></span>
                <div>
                  <h2>显示模式</h2>
                  <p>独立于皮肤选择明暗色彩模式</p>
                </div>
              </div>

              <div class="zen-choice-grid three">
                <button type="button" class="zen-choice-card" :aria-pressed="themeMode === 'system'" @click="setThemeMode('system')">
                  <strong>跟随系统</strong>
                  <p>自动匹配操作系统深浅色偏好。</p>
                </button>
                <button type="button" class="zen-choice-card" :aria-pressed="themeMode === 'light'" @click="setThemeMode('light')">
                  <strong>浅色模式</strong>
                  <p>明亮高清晰日间工作台。</p>
                </button>
                <button type="button" class="zen-choice-card" :aria-pressed="themeMode === 'dark'" @click="setThemeMode('dark')">
                  <strong>深色模式</strong>
                  <p>低眩光暗色画布，夜间更护眼。</p>
                </button>
              </div>

              <p class="zen-pref-note">外观与主题偏好自动保存于当前浏览器，即时生效，无需提交服务端。</p>

              <div v-if="token" class="zen-nested-box zen-session-box">
                <div class="zen-nested-copy">
                  <strong>当前浏览器登录状态</strong>
                  <p>退出后会清除此浏览器保存的访问密码，并要求重新验证。</p>
                </div>
                <button type="button" class="button secondary" @click="logout">
                  <AppIcon name="logout" :size="15" />
                  <span>退出登录</span>
                </button>
              </div>
            </section>
          </div>

          <div v-if="settingsTab !== 'appearance'" class="zc-savebar">
            <p v-if="settingsError" class="form-error" role="alert">{{ settingsError }}</p>
            <span v-else class="zc-save-hint">修改保存后立即同步至下载器服务端</span>
            <button type="submit" class="button primary" :disabled="settingsBusy || connection !== 'online'">
              <span v-if="settingsBusy" class="spinner" />
              {{ settingsBusy ? '保存中…' : '保存设置' }}
            </button>
          </div>
        </form>
      </template>
    </main>

    <nav class="zen-mobile-tabbar" aria-label="移动端导航">
      <button
        type="button"
        class="zen-mobile-tab"
        :aria-current="view === 'tasks' && !newDownloadOpen ? 'page' : undefined"
        @click="closeModal(); view = 'tasks'"
      >
        <AppIcon name="layers" :size="20" />
        <span>任务</span>
        <span v-if="active.length" class="zen-mobile-badge">{{ active.length }}</span>
      </button>
      <button
        type="button"
        class="zen-mobile-tab primary-action"
        :aria-current="newDownloadOpen ? 'page' : undefined"
        @click="newDownload('download')"
      >
        <AppIcon name="plus" :size="20" />
        <span>新建</span>
      </button>
      <button
        type="button"
        class="zen-mobile-tab"
        :aria-current="view === 'rss' && !newDownloadOpen ? 'page' : undefined"
        @click="closeModal(); openRss(true)"
      >
        <AppIcon name="rss" :size="20" />
        <span>订阅</span>
        <span v-if="rssUnreadTotal" class="zen-mobile-badge">{{ rssUnreadTotal }}</span>
      </button>
      <button
        type="button"
        class="zen-mobile-tab"
        :aria-current="view === 'scrape' && !newDownloadOpen ? 'page' : undefined"
        @click="closeModal(); view = 'scrape'"
      >
        <AppIcon name="film" :size="20" />
        <span>刮削</span>
        <span v-if="scrapeJob?.status === 'running'" class="zen-mobile-badge">{{ scrapePercent }}%</span>
      </button>
      <button
        type="button"
        class="zen-mobile-tab"
        :aria-current="view === 'settings' && !newDownloadOpen ? 'page' : undefined"
        @click="closeModal(); openSettings()"
      >
        <AppIcon name="settings" :size="20" />
        <span>设置</span>
      </button>
    </nav>

    <div v-if="notice" class="toast" :class="{ error: noticeError }" role="status">
      <AppIcon :name="noticeError ? 'info' : 'check'" :size="18" />
      <span>{{ notice }}</span>
      <button type="button" class="icon-button" aria-label="关闭提示" @click="notice = ''"><AppIcon name="close" :size="16" /></button>
    </div>

    <dialog
      ref="modalRef"
      class="dialog"
      :class="{ 'zen-drawer': !!detailsUrl, 'zen-dialog': !detailsUrl, 'login-dialog': loginOpen, 'entry-dialog': newDownloadOpen, 'removal-dialog': removeTasks.length || !!rssDeleteSource, 'rss-dialog': rssEditorOpen }"
      :aria-label="loginOpen ? '服务访问密码' : newDownloadOpen ? entryTitle : detailsUrl ? '下载详情' : editTask ? '修改请求头' : rssEditorOpen ? (rssEditingSource ? '编辑订阅源' : '添加 RSS 订阅源') : rssDeleteSource ? '删除订阅源' : '移除任务'"
      @cancel.prevent="closeModal"
      @keydown.esc.prevent="closeModal"
      @click="($event.target === modalRef) && closeModal()"
    >
      <form v-if="modalOpen" @submit.prevent="modalSubmit">
        <div class="dialog-header">
          <div class="dialog-heading">
            <span class="zen-icon dialog-symbol" :class="removeTasks.length || rssDeleteSource ? 'zen-icon-orange' : rssEditorOpen ? 'zen-icon-orange' : entryMode === 'extract' && newDownloadOpen ? 'zen-icon-purple' : 'zen-icon-blue'">
              <AppIcon :name="loginOpen ? 'bolt' : newDownloadOpen ? (entryMode === 'extract' ? 'link' : 'plus') : detailsUrl ? 'file' : editTask ? 'edit' : rssEditorOpen ? 'rss' : 'trash'" :size="20" />
            </span>
            <div>
              <h2>{{ loginOpen ? '连接 StreamDock' : newDownloadOpen ? entryTitle : detailsUrl ? (detailsTask && isFile(detailsTask) ? '文件详情' : '下载分片') : editTask ? '修改请求头并继续' : rssEditorOpen ? (rssEditingSource ? `编辑订阅源：${rssEditingSource.name}` : '添加 RSS 订阅源') : rssDeleteSource ? `删除订阅源「${rssDeleteSource.name}」？` : removeTasks.length > 1 ? `移除所选 ${removeTasks.length} 个任务？` : '移除这个任务？' }}</h2>
              <p>{{ loginOpen ? '输入密码以连接服务' : newDownloadOpen ? (entryMode === 'extract' ? '先查找媒体，再选择需要下载的内容' : '粘贴资源地址或平台分享文本，支持批量添加') : detailsUrl ? '查看下载进度与任务状态' : editTask ? '更新访问凭据，保留已下载的缓存' : rssEditorOpen ? '拉取完整订阅列表并配置规则过滤与自动 NFO 刮削' : rssDeleteSource ? '将清理该订阅源及其全部缓存条目' : '选择需要清理的内容，成品文件默认保留' }}</p>
            </div>
          </div>
          <button v-if="!loginOpen" type="button" class="icon-button" aria-label="关闭对话框" :disabled="modalBusy" @click="closeModal"><AppIcon name="close" /></button>
        </div>

        <div class="dialog-body">
          <template v-if="loginOpen">
            <p class="dialog-intro">服务已启用密码保护。勾选下方选项后，同一浏览器下次打开无需重复输入。</p>
            <label for="password">访问密码</label>
            <input id="password" v-model="password" type="password" autocomplete="current-password" required autofocus />
            <label class="check-field">
              <input v-model="rememberLogin" type="checkbox" />
              <span>在此浏览器记住登录状态<small>取消勾选则仅在当前标签页会话有效。</small></span>
            </label>
          </template>

          <template v-else-if="newDownloadOpen">
            <div class="zen-segmented entry-modes">
              <button type="button" :class="{ 'is-active': entryMode === 'download' }" :aria-pressed="entryMode === 'download'" @click="switchEntry('download')">直链 / 分享链接</button>
              <button type="button" :class="{ 'is-active': entryMode === 'extract' }" :aria-pressed="entryMode === 'extract'" @click="switchEntry('extract')">网页提取</button>
            </div>

            <div class="input-caption">
              <label for="urls">{{ entryMode === 'extract' ? '网页或分享链接' : '下载链接' }} <span class="field-note">{{ entryMode === 'extract' ? '最多 20 个网页' : '一行一个，最多 100 个' }}</span></label>
              <span v-if="lines.length" class="link-count">{{ lines.length }} 个链接</span>
            </div>
            <textarea id="urls" ref="inputRef" v-model="urls" class="url-input" rows="5" :placeholder="entryMode === 'extract' ? 'https://example.com/watch/123' : 'https://example.com/index.m3u8'" :disabled="extractBusy" required spellcheck="false" autocapitalize="off" />

            <template v-if="entryMode === 'download'">
              <div class="form-grid download-options">
                <div>
                  <label for="download-type">下载类型</label>
                  <select id="download-type" v-model="downloadType">
                    <option value="auto">自动识别（含平台分享链接）</option>
                    <option value="m3u8">M3U8 / HLS 播放列表</option>
                    <option value="file">普通文件直链（MP4、VTT 等）</option>
                    <option value="parser">平台解析（含内置签名站点）</option>
                  </select>
                </div>
                <div>
                  <label for="filename">文件名称 <span class="field-note">可选</span></label>
                  <input id="filename" v-model="title" placeholder="不填则自动生成" maxlength="150" />
                </div>
              </div>
              <p class="field-help">支持 M3U8、MP4、VTT 与抖音 / 微博 / 皮皮虾及内置签名站点页面链接。无后缀文件请选择「普通文件」，伪装后缀的播放列表请选择「M3U8」。微博需填写 Cookie。</p>
            </template>

            <template v-else>
              <div class="form-grid extraction-options">
                <div>
                  <label for="extract-depth">搜索深度</label>
                  <select id="extract-depth" v-model.number="extractDepth" :disabled="extractBusy">
                    <option :value="0">仅当前页面</option>
                    <option :value="1">向下 1 层</option>
                    <option :value="2">向下 2 层</option>
                    <option :value="3">向下 3 层</option>
                  </select>
                </div>
                <div>
                  <label for="extract-path">子页面路径 <span class="field-note">可选</span></label>
                  <input id="extract-path" v-model="extractPath" :disabled="extractBusy" placeholder="例如 /play/" />
                </div>
              </div>
              <p class="field-help">搜索站内链接和播放器 iframe，最多读取 30 页；不会执行网页脚本。</p>
              <button type="button" class="button secondary extract-run" :disabled="!lines.length || modalBusy" @click="runExtraction">
                <span v-if="extractBusy" class="spinner" />
                <AppIcon v-else name="search" :size="16" />
                {{ extractBusy ? '取消提取' : '解析并提取' }}
              </button>

              <section v-if="extracted" class="extraction-result" aria-label="提取结果">
                <div class="extraction-summary">
                  <label class="task-checkbox">
                    <input type="checkbox" :checked="!!extracted.items.length && selectedExtracted.length === extracted.items.length" :indeterminate="selectedExtracted.length > 0 && selectedExtracted.length < extracted.items.length" :disabled="!extracted.items.length" @change="toggleExtractedAll" />
                    <span>找到 {{ extracted.items.length }} 个，已选 {{ selectedExtracted.length }} 个</span>
                  </label>
                  <small>已读取 {{ extracted.pagesVisited }} 页</small>
                </div>
                <p v-if="!extracted.items.length" class="field-help">未找到可下载的媒体。可调整深度、补充 Cookie，或使用实际播放页地址。</p>
                <ul v-else class="extracted-list">
                  <li v-for="item in extracted.items" :key="item.url">
                    <label class="extracted-item">
                      <input v-model="extractedSelection" type="checkbox" :value="item.url" />
                      <span>
                        <strong>{{ item.title || host(item.url) }}</strong>
                        <small>{{ item.platform || item.type.toUpperCase() }} · {{ host(item.source) }}</small>
                        <span class="extracted-url" :title="item.url">{{ item.url }}</span>
                      </span>
                    </label>
                  </li>
                </ul>
                <p v-if="extracted.truncated" class="field-help">已达到提取上限，可缩小范围后继续提取。</p>
                <details v-if="extracted.errors.length" class="extraction-errors">
                  <summary>{{ extracted.errors.length }} 个页面提取失败，查看原因</summary>
                  <p v-for="error in extracted.errors" :key="error.url">{{ host(error.url) }}：{{ error.message }}</p>
                </details>
              </section>
            </template>

            <button type="button" class="advanced-toggle" :aria-expanded="advanced" aria-controls="request-options" @click="advanced = !advanced">
              <span>请求头设置 <small>自动携带 Referer 与 User-Agent</small></span>
              <AppIcon name="chevron" :size="16" :class="{ rotated: advanced }" />
            </button>
            <div v-show="advanced" id="request-options">
              <label for="referer">来源页 Referer</label>
              <input id="referer" v-model="source" type="url" :placeholder="originHint" />
              <p class="field-help">有防盗链时填视频的实际播放页面；留空使用资源域名。</p>
              <label for="headers">附加请求头</label>
              <textarea id="headers" v-model="headers" class="mono" rows="3" placeholder="Cookie: ...&#10;User-Agent: ..." spellcheck="false" />
              <p class="field-help">支持每行「名称: 值」或 JSON，同名字段覆盖默认设置。</p>
            </div>
          </template>

          <template v-else-if="detailsUrl && detailsTask">
            <div class="detail-file">
              <span class="zen-format-badge" :class="`tone-${formatTone(detailsTask)}`">{{ kind(detailsTask) }}</span>
              <strong>{{ name(detailsTask) }}</strong>
              <span class="zen-status" :class="detailsTask.status">{{ label(detailsTask) }}</span>
            </div>
            <p v-if="detailsTask.options?.sourceUrl" class="field-help">来源详情页：<a :href="detailsTask.options.sourceUrl" target="_blank" rel="noreferrer noopener">{{ detailsTask.options.sourceUrl }}</a></p>

            <template v-if="isFile(detailsTask)">
              <dl class="file-details">
                <div><dt>文件类型</dt><dd>{{ kind(detailsTask) }} 直链文件</dd></div>
                <div><dt>下载大小</dt><dd>{{ bytes(detailsTask.downloadedSize) }} / {{ detailsTask.size ? bytes(detailsTask.size) : '未知' }}</dd></div>
                <div><dt>下载速度</dt><dd>{{ detailsTask.status === 'resume' ? detailsTask.speedDesc || '0 B/s' : '—' }}</dd></div>
              </dl>
              <p v-if="detailsTask.errmsg" class="form-error">{{ detailsTask.errmsg }}</p>
              <p class="field-help">按原始内容保存。服务器支持 Range 和文件标识时可断点续传。</p>
            </template>

            <template v-else>
              <div class="segment-summary">
                <strong>已下载分片 <b>{{ segmentCounts.done }}</b><span>/ {{ segments.items.length || detailsTask.tsCount || 0 }}</span></strong>
                <span>{{ bytes(detailsTask.downloadedSize) }}</span>
              </div>
              <fieldset class="zen-segmented segment-legend">
                <legend class="sr-only">筛选分片</legend>
                <button type="button" :class="{ 'is-active': segmentFilter === 'all' }" @click="filterSegments('all')">全部</button>
                <button type="button" class="done" :class="{ 'is-active': segmentFilter === 'done' }" @click="filterSegments('done')"><i />已完成 {{ segmentCounts.done }}</button>
                <button type="button" class="error" :class="{ 'is-active': segmentFilter === 'error' }" @click="filterSegments('error')"><i />失败 {{ segmentCounts.error }}</button>
                <button type="button" class="pending" :class="{ 'is-active': segmentFilter === 'pending' }" @click="filterSegments('pending')"><i />未完成 {{ segmentCounts.pending }}</button>
              </fieldset>
              <p v-if="segmentError" class="form-error" role="alert">{{ segmentError }}</p>
              <div v-if="segmentLoading" class="segment-empty"><span class="spinner" />正在读取分片状态…</div>
              <div v-else-if="!pageSegments.length" class="segment-empty">{{ segments.items.length ? '没有符合条件的分片' : '解析完成后将在这里显示分片状态' }}</div>
              <div v-else class="segment-grid">
                <button v-for="segment in pageSegments" :key="segment.index" type="button" :class="[segment.status, { inspected: selectedSegment === segment.index }]" :aria-label="`分片 ${segment.index + 1}：${segmentLabels[segment.status]}`" :title="segment.error || `分片 ${segment.index + 1} · ${segmentLabels[segment.status]}`" @click="selectedSegment = segment.index">{{ segment.index + 1 }}</button>
              </div>
              <div v-if="segmentPages > 1" class="segment-pagination">
                <button type="button" class="button secondary" :disabled="segmentPage === 1" @click="segmentPage--">上一页</button>
                <span>{{ segmentPage }} / {{ segmentPages }}</span>
                <button type="button" class="button secondary" :disabled="segmentPage === segmentPages" @click="segmentPage++">下一页</button>
              </div>
              <div v-if="inspectedSegment" class="segment-inspector">
                <strong>分片 {{ inspectedSegment.index + 1 }} · {{ segmentLabels[inspectedSegment.status] }}</strong>
                <span>{{ bytes(inspectedSegment.size) }} · {{ inspectedSegment.attempts }} 次下载尝试</span>
                <p v-if="inspectedSegment.error">{{ inspectedSegment.error }}</p>
              </div>
              <p class="field-help">绿色已完成，蓝色正在下载，红色失败，灰色尚未完成。点击编号可查看详情。</p>
            </template>
          </template>

          <template v-else-if="editTask">
            <p class="dialog-intro">分片缓存会保留。更新后将自动继续下载。</p>
            <p class="edit-url">{{ editTask.url }}</p>
            <label for="edit-headers">本任务请求头</label>
            <textarea id="edit-headers" v-model="editHeaders" rows="7" class="mono" placeholder="Referer: https://example.com/" spellcheck="false" />
            <p class="field-help">支持 JSON 或每行「名称: 值」。不填时使用默认设置和自动请求头。</p>
          </template>

          <template v-else-if="rssEditorOpen">
            <div class="zen-segmented zen-rss-editor-tabs" role="tablist" aria-label="订阅源设置标签">
              <button type="button" role="tab" :aria-selected="rssEditorTab === 'basic'" :class="{ 'is-active': rssEditorTab === 'basic' }" @click="rssEditorTab = 'basic'">基本</button>
              <button type="button" role="tab" :aria-selected="rssEditorTab === 'filter'" :class="{ 'is-active': rssEditorTab === 'filter' }" @click="rssEditorTab = 'filter'">过滤</button>
              <button type="button" role="tab" :aria-selected="rssEditorTab === 'advanced'" :class="{ 'is-active': rssEditorTab === 'advanced' }" @click="rssEditorTab = 'advanced'">高级与 NFO</button>
            </div>

            <template v-if="rssEditorTab === 'basic'">
              <label for="rss-form-mode">来源类型</label>
              <select id="rss-form-mode" v-model="rssForm.mode">
                <option value="rss">RSS / Atom 订阅 (XML)</option>
                <option value="html">公开列表页 (HTML)</option>
              </select>

              <label for="rss-form-url">订阅地址</label>
              <div class="zen-rss-validate-row">
                <input id="rss-form-url" v-model="rssForm.url" type="url" placeholder="https://example.com/rss.xml" required />
                <button type="button" class="button secondary" :disabled="!rssForm.url.trim() || rssValidateBusy || modalBusy" @click="validateRssUrl">
                  <span v-if="rssValidateBusy" class="spinner mini" />
                  <span>{{ rssValidateBusy ? '验证中…' : '验证并预览' }}</span>
                </button>
              </div>
              <p v-if="rssValidateError" class="form-error" role="alert">{{ rssValidateError }}</p>
              <p v-else-if="rssValidateResult" class="field-help">验证成功：{{ rssValidateResult.feedTitle }}（共 {{ rssValidateResult.totalItems }} 条）</p>

              <label for="rss-form-name">显示名称 <span class="field-note">留空自动使用域名或 Feed 标题</span></label>
              <input id="rss-form-name" v-model="rssForm.name" maxlength="80" placeholder="例如 订阅源名称" />

              <div class="form-grid">
                <div>
                  <label for="rss-form-interval">抓取间隔</label>
                  <select id="rss-form-interval" v-model.number="rssForm.intervalMinutes">
                    <option :value="10">每 10 分钟</option>
                    <option :value="30">每 30 分钟</option>
                    <option :value="60">每 1 小时</option>
                    <option :value="120">每 2 小时</option>
                    <option :value="360">每 6 小时</option>
                    <option :value="720">每 12 小时</option>
                    <option :value="1440">每 24 小时</option>
                  </select>
                </div>
                <div>
                  <label for="rss-form-max">每轮自动下载上限 (1–100)</label>
                  <input id="rss-form-max" v-model.number="rssForm.maxPerFetch" type="number" min="1" max="100" required />
                </div>
              </div>

              <label class="check-field zen-nested-check">
                <input v-model="rssForm.enabled" type="checkbox" />
                <span>启用定时抓取<small>停用后保留已拉取历史条目，停止后台定时请求。</small></span>
              </label>

              <label class="check-field zen-nested-check">
                <input v-model="rssForm.autoDownload" type="checkbox" />
                <span>自动下载新命中条目<small>默认关闭（收集模式：历史全部入库为新条目供手动勾选）；开启后首轮历史标为“历史跳过”，仅自动下此后新增条目。</small></span>
              </label>
            </template>

            <template v-else-if="rssEditorTab === 'filter'">
              <label for="rss-form-include">包含关键词 <span class="field-note">“|”表示或，“空格”表示且</span></label>
              <input id="rss-form-include" v-model="rssForm.includePattern" placeholder="例如 1080p 高清|完整版" />

              <label for="rss-form-exclude">排除关键词</label>
              <input id="rss-form-exclude" v-model="rssForm.excludePattern" placeholder="例如 预告|花絮" />

              <div class="form-grid">
                <div>
                  <label for="rss-form-sizemin">体积下限 <span class="field-note">仅对已知体积生效</span></label>
                  <input id="rss-form-sizemin" v-model="rssForm.sizeMinText" placeholder="例如 200 MB" />
                </div>
                <div>
                  <label for="rss-form-sizemax">体积上限</label>
                  <input id="rss-form-sizemax" v-model="rssForm.sizeMaxText" placeholder="例如 4 GB" />
                </div>
              </div>

              <label for="rss-form-pattern">详情页链接筛选词 <span class="field-note">可选</span></label>
              <input id="rss-form-pattern" v-model="rssForm.itemPattern" placeholder="例如 /v/ 或 /watch/" />

              <label class="check-field zen-nested-check">
                <input v-model="rssForm.useRegex" type="checkbox" />
                <span>包含 / 排除按正则表达式解析<small>语法错误时安全放行，避免静默漏下。</small></span>
              </label>

              <label class="check-field zen-nested-check">
                <input v-model="rssForm.smartEpisode" type="checkbox" />
                <span>智能剧集去重<small>识别 S01E02 / 第12集 / EP12 等集数键，同集只下载一次。</small></span>
              </label>

              <div v-if="rssFilterPreview.total || rssFilterPreview.sizeError" class="zen-rss-preview-box">
                <p v-if="rssFilterPreview.sizeError" class="form-error" role="alert">{{ rssFilterPreview.sizeError }}</p>
                <template v-else>
                  <div class="zen-rss-preview-head">
                    <span>样本过滤预览（共 {{ rssFilterPreview.total }} 条）</span>
                    <span>通过 {{ rssFilterPreview.accepted }} / 拦截 {{ rssFilterPreview.rejected }}</span>
                  </div>
                  <ul class="zen-rss-preview-list">
                    <li v-for="row in rssFilterPreview.rows" :key="row.guid">
                      <span :title="row.title">{{ row.title }}</span>
                      <b :class="row.pass ? 'pass' : 'drop'">{{ row.label }}</b>
                    </li>
                  </ul>
                </template>
              </div>
              <p v-else class="field-help">可在「基本」标签页点击「验证并预览」获取上游样本，实时预览过滤规则效果。</p>
            </template>

            <template v-else>
              <label for="rss-form-savedir">下载保存目录 <span class="field-note">留空使用全局默认目录</span></label>
              <input id="rss-form-savedir" v-model="rssForm.saveDir" :placeholder="config.saveDir || './downloads'" />

              <div class="form-grid">
                <div>
                  <label for="rss-form-referer">来源页 Referer <span class="field-note">可选</span></label>
                  <input id="rss-form-referer" v-model="rssForm.referer" type="url" placeholder="留空自动使用站点域名" />
                </div>
                <div>
                  <label for="rss-form-ua">自定义 User-Agent <span class="field-note">可选</span></label>
                  <input id="rss-form-ua" v-model="rssForm.userAgent" placeholder="留空使用默认浏览器 UA" />
                </div>
              </div>

              <label for="rss-form-cookies">站点 Cookie <span class="field-note">仅向订阅源同域请求发送</span></label>
              <textarea id="rss-form-cookies" v-model="rssForm.cookies" rows="2" class="mono" placeholder="session=...; token=..." spellcheck="false" />

              <label class="check-field zen-nested-check">
                <input v-model="rssForm.autoScrape" type="checkbox" />
                <span>下载合并完成后自动生成 Emby NFO 与海报<small>入队时精确绑定条目详情页地址，视频完成后直接精准刮削。</small></span>
              </label>

              <template v-if="rssForm.autoScrape">
                <label for="rss-scrape-mode">NFO 整理方式</label>
                <select id="rss-scrape-mode" v-model="rssForm.scrapeMode">
                  <option value="in-place">原地刮削（同名 .nfo + 封面）</option>
                  <option value="organize">按目录归档（独立影片目录）</option>
                </select>

                <template v-if="rssForm.scrapeMode === 'organize'">
                  <label for="rss-scrape-outdir">归档输出目录</label>
                  <input id="rss-scrape-outdir" v-model="rssForm.scrapeOutputDir" placeholder="/app/downloads/emby" required />
                  <div class="form-grid">
                    <div>
                      <label for="rss-scrape-transfer">文件处理</label>
                      <select id="rss-scrape-transfer" v-model="rssForm.scrapeTransferMode">
                        <option value="move">移动文件</option>
                        <option value="hardlink">硬链接（同分区零拷贝）</option>
                        <option value="copy">复制文件</option>
                      </select>
                    </div>
                    <div>
                      <label for="rss-scrape-folder">目录结构</label>
                      <select id="rss-scrape-folder" v-model="rssForm.scrapeFolderTemplate">
                        <option value="actor-title">演员 / 日期+片名 / 视频</option>
                        <option value="title">日期+片名 / 视频</option>
                      </select>
                    </div>
                  </div>
                </template>

                <label for="rss-scrape-crop">封面海报裁切（2:3 竖版 poster）</label>
                <select id="rss-scrape-crop" v-model="rssForm.scrapePosterCrop">
                  <option value="center">2:3 居中裁切海报（保留原横图）</option>
                  <option value="right">2:3 靠右裁切海报（保留原横图）</option>
                  <option value="none">不裁切（原横图共用）</option>
                </select>

                <label class="check-field zen-nested-check">
                  <input v-model="rssForm.scrapeDownloadImages" type="checkbox" />
                  <span>自动下载封面与生成 2:3 竖版海报<small>自动携带上游 Referer 拉取封面图。</small></span>
                </label>
              </template>
            </template>
          </template>

          <template v-else-if="rssDeleteSource">
            <p class="dialog-intro">删除「{{ rssDeleteSource.name }}」会移除该订阅源及其全部缓存条目。</p>
            <p class="field-help">已加入下载队列的任务和本地已下载视频文件不会被删除。</p>
          </template>

          <template v-else-if="removeTasks.length">
            <p class="dialog-intro">{{ removeTasks.length === 1 ? name(removeTasks[0]) : `将移除所选的 ${removeTasks.length} 个任务，正在运行的任务会先停止。` }}</p>
            <ul v-if="removeTasks.length > 1" class="removal-list"><li v-for="task in removeTasks" :key="task.url">{{ name(task) }}</li></ul>
            <label class="check-field"><input v-model="removeCache" type="checkbox" /><span>同时清理已下载的分片缓存</span></label>
            <label v-if="removeTasks.some(task => task.localVideo)" class="check-field"><input v-model="removeFile" type="checkbox" /><span>同时删除服务器上的成品文件<small>勾选后文件无法恢复。</small></span></label>
            <p class="field-help">未勾选的文件会保留在服务器上。</p>
          </template>

          <p v-if="modalError" class="form-error" role="alert">{{ modalError }}</p>
        </div>

        <div v-if="detailsUrl" class="dialog-footer detail-footer">
          <span v-if="detailsTask && !isFile(detailsTask) && !detailsTask.browserCapture" class="detail-live"><i />每秒自动更新</span>
          <button v-if="detailsTask?.status === 'error' && segmentCounts.error" type="button" class="button primary" :disabled="pending.has(detailsTask.url)" @click="action(detailsTask, 'retry-failed')"><AppIcon name="retry" :size="16" />仅重试失败分片 ({{ segmentCounts.error }})</button>
          <button type="button" class="button secondary" @click="closeModal">关闭</button>
        </div>
        <div v-else class="dialog-footer">
          <button v-if="!loginOpen" type="button" class="button secondary" :disabled="modalBusy" @click="closeModal">取消</button>
          <button type="submit" class="button" :class="removeTasks.length || rssDeleteSource ? 'danger' : 'primary'" :disabled="modalBusy || extractBusy || (newDownloadOpen && (connection !== 'online' || (entryMode === 'extract' && !selectedExtracted.length)))">
            <span v-if="modalBusy" class="spinner" />
            {{ modalBusy ? '请稍候…' : loginOpen ? '连接服务' : newDownloadOpen ? (entryMode === 'extract' ? `下载所选 (${selectedExtracted.length})` : lines.length > 1 ? `添加 ${lines.length} 个下载` : '开始下载') : editTask ? '保存并继续' : rssEditorOpen ? (rssEditingSource ? '保存修改' : '添加订阅源') : rssDeleteSource ? '确认删除' : '确认移除' }}
          </button>
        </div>
      </form>
    </dialog>
  </div>
</template>
