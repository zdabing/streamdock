/** Shared REST/WebSocket data, free of server dependencies. */
export type TaskStatus = 'pause' | 'resume' | 'done' | 'pending' | 'error';
export type TaskPhase = 'queued' | 'parsing' | 'downloading' | 'merging' | 'done';
export type SegmentStatus = 'pending' | 'downloading' | 'done' | 'error';
export interface SegmentProgress {
  index: number;
  status: SegmentStatus;
  size: number;
  attempts: number;
  error?: string;
}
export interface SegmentSnapshot {
  fingerprint: string;
  items: SegmentProgress[];
}
export interface DownloadStats {
  downloadType?: 'm3u8' | 'file';
  startTime?: number;
  endTime?: number;
  progress?: number;
  phase?: TaskPhase;
  mergeProgress?: number;
  mergeMessage?: string;
  tsCount?: number;
  tsSuccess?: number;
  tsFailed?: number;
  hasSegmentDetails?: boolean;
  duration?: number;
  size?: number;
  downloadedSize?: number;
  speed?: number;
  speedDesc?: string;
  remainingTime?: number;
  localVideo?: string;
  filename?: string;
  threadNum?: number;
  errmsg?: string;
}
export interface TaskScrapeMeta {
  pageUrl: string;
  title?: string;
  pubDate?: string;
  tags?: string[];
  guid?: string;
  referer?: string;
  studio?: string;
  mode?: ScrapeMode;
  outputDir?: string;
  transferMode?: ScrapeTransferMode;
  folderTemplate?: ScrapeFolderTemplate;
  posterCrop?: ScrapePosterCrop;
  downloadImages?: boolean;
  overwriteNfo?: boolean;
}
export interface TaskOptions {
  useDefaultHeaders?: boolean;
  type?: 'auto' | 'm3u8' | 'file' | 'parser';
  sourceUrl?: string;
  headerSourceUrl?: string;
  filename?: string;
  saveDir?: string;
  ignoreSegments?: string;
  headers?: string | Record<string, string | string[] | number | undefined>;
  scrapeMeta?: TaskScrapeMeta;
}
export interface TaskItem extends DownloadStats {
  url: string;
  status: TaskStatus;
  browserCapture?: boolean;
  browserSource?: string;
  cacheDir?: string;
  options?: TaskOptions;
  dlOptions?: TaskOptions;
  scrapeMeta?: TaskScrapeMeta;
  scrapeStatus?: 'idle' | 'scraping' | 'done' | 'error';
  scrapeMessage?: string;
  nfoPath?: string;
  posterPath?: string;
}
export interface QueueStatus {
  queueLength: number;
  activeDownloads: string[];
  maxConcurrent: number;
}
export interface ServerInfo {
  version: string;
}
export interface ServerConfig {
  threadNum: number;
  saveDir: string;
  delCache: boolean;
  maxDownloads: number;
  ffmpegPath: string;
  headers: string;
  mergeTimeout: number;
  proxyMode: 'custom' | 'system' | 'disabled';
  proxyUrl: string;
  noProxy: string;
}
export interface ApiResponse<T = unknown> {
  code: number;
  message?: string;
  data?: T;
}
export interface VideoInfo {
  url: string;
  title: string;
  referer?: string;
  platform?: string;
  author?: string;
  avatar?: string;
  time?: number;
  uid?: string;
  like?: number;
  cover?: string;
  music?: { author: string; avatar: string };
}
export interface ExtractedMedia {
  url: string;
  title: string;
  source: string;
  credentialSource?: string;
  type: 'm3u8' | 'file';
  platform?: string;
}
export interface ExtractionResult {
  items: ExtractedMedia[];
  errors: { url: string; message: string }[];
  pagesVisited: number;
  truncated: boolean;
}
export type ScrapeMode = 'in-place' | 'organize';
export type ScrapeTransferMode = 'move' | 'hardlink' | 'copy';
export type ScrapeTransferActual = 'none' | 'rename' | 'copy-cross-fs' | 'hardlink' | 'copy';
export type ScrapePosterCrop = 'center' | 'right' | 'none';
export type ScrapeFolderTemplate = 'actor-title' | 'title';
export interface ScrapeItemTiming {
  detailMs: number;
  transferMs: number;
  artworkMs: number;
  cropMs: number;
  totalMs: number;
  transferActual?: ScrapeTransferActual;
}
export type ScrapeSourceType = 'rss' | 'wordpress';
export interface ScrapeDataSource {
  id: string;
  name: string;
  type: ScrapeSourceType;
  url: string;
  enabled: boolean;
  referer?: string;
}
export interface ScrapeScanSourceSummary {
  id: string;
  name: string;
  type: ScrapeSourceType;
  url: string;
  catalogCount: number;
  queriedCount: number;
  matchedCount: number;
  error?: string;
}
export interface ScrapeCandidate {
  guid: string;
  title: string;
  pageUrl: string;
  pubDate: string;
  tags: string[];
  score: number;
  sourceId?: string;
  sourceName?: string;
  sourceType?: ScrapeSourceType;
  actors?: string[];
  studio?: string;
  coverUrl?: string;
  referer?: string;
}
export interface ScrapePreviewItem {
  filePath: string;
  relativePath: string;
  filename: string;
  size: number;
  hasNfo: boolean;
  status: 'matched' | 'low-confidence' | 'unmatched' | 'skipped-nfo';
  match?: ScrapeCandidate;
  candidates: ScrapeCandidate[];
}
export interface ScrapeScanRequest {
  scanDir?: string;
  scanDirs?: string[];
  sources?: ScrapeDataSource[];
  rssUrl?: string;
  feedId?: string;
  referer?: string;
  recursive?: boolean;
  skipExistingNfo?: boolean;
  minScore?: number;
  refreshRss?: boolean;
}
export interface ScrapeScanResult {
  scanDir: string;
  scanDirs: string[];
  rssUrl: string;
  feedTitle: string;
  catalogCount: number;
  scannedFiles: number;
  matchedCount: number;
  sourceSummaries?: ScrapeScanSourceSummary[];
  items: ScrapePreviewItem[];
}
export interface ScrapeConfig {
  scanDirs: string[];
  sources: ScrapeDataSource[];
  feedPreset: string;
  rssUrl: string;
  referer: string;
  mode: ScrapeMode;
  outputDir: string;
  transferMode: ScrapeTransferMode;
  folderTemplate: ScrapeFolderTemplate;
  posterCrop: ScrapePosterCrop;
  minScore: number;
  recursive: boolean;
  skipExistingNfo: boolean;
  downloadImages: boolean;
  overwriteNfo: boolean;
  concurrency: number;
}
export interface ScrapeRunItemInput {
  filePath: string;
  pageUrl: string;
  title?: string;
  pubDate?: string;
  tags?: string[];
  guid?: string;
  actors?: string[];
  studio?: string;
  coverUrl?: string;
  referer?: string;
}
export interface ScrapeRunRequest {
  items: ScrapeRunItemInput[];
  mode: ScrapeMode;
  outputDir?: string;
  transferMode?: ScrapeTransferMode;
  folderTemplate?: ScrapeFolderTemplate;
  posterCrop?: ScrapePosterCrop;
  downloadImages?: boolean;
  overwriteNfo?: boolean;
  referer?: string;
  studio?: string;
  concurrency?: number;
}
export interface ScrapeItemOutcome {
  filePath: string;
  targetVideoPath?: string;
  nfoPath?: string;
  posterPath?: string;
  fanartPath?: string;
  thumbPath?: string;
  title?: string;
  actor?: string;
  status: 'success' | 'failed';
  message?: string;
  failedStage?: string;
  timing?: ScrapeItemTiming;
}
export interface ScrapeRunResult {
  total: number;
  succeeded: number;
  failed: number;
  results: ScrapeItemOutcome[];
}
export type ScrapeJobStatus = 'idle' | 'running' | 'done' | 'stopped' | 'error';
export interface ScrapeJobTimingStats {
  count: number;
  avgTotalMs: number;
  avgDetailMs: number;
  avgTransferMs: number;
  avgArtworkMs: number;
  avgCropMs: number;
  crossFsCount: number;
}
export interface ScrapeJobSnapshot {
  id: string;
  revision?: number;
  status: ScrapeJobStatus;
  mode: ScrapeMode;
  outputDir?: string;
  transferMode: ScrapeTransferMode;
  concurrency: number;
  total: number;
  done: number;
  succeeded: number;
  failed: number;
  remaining: number;
  canResume: boolean;
  canRetryFailed: boolean;
  startedAt: number;
  finishedAt?: number;
  elapsedMs?: number;
  currentFile?: string;
  error?: string;
  timing?: ScrapeJobTimingStats;
  results: ScrapeItemOutcome[];
  totalResults: number;
  succeededPaths?: string[];
}
export type RssSourceMode = 'rss' | 'html';
export type RssItemStatus = 'new' | 'downloaded' | 'ignored' | 'filtered' | 'duplicate-episode' | 'seed-skipped';
export type RssRejectReason = '' | 'not_included' | 'excluded' | 'too_small' | 'too_large' | 'dup_episode' | 'seed_skipped';
export interface RssSource {
  id: string;
  name: string;
  mode: RssSourceMode;
  url: string;
  enabled: boolean;
  autoDownload: boolean;
  intervalMinutes: number;
  maxPerFetch: number;
  itemPattern: string;
  includePattern: string;
  excludePattern: string;
  useRegex: boolean;
  smartEpisode: boolean;
  sizeMinBytes: number;
  sizeMaxBytes: number;
  saveDir: string;
  referer: string;
  cookies: string;
  userAgent: string;
  autoScrape: boolean;
  scrapeMode: ScrapeMode;
  scrapeOutputDir: string;
  scrapeTransferMode: ScrapeTransferMode;
  scrapeFolderTemplate: ScrapeFolderTemplate;
  scrapePosterCrop: ScrapePosterCrop;
  scrapeDownloadImages: boolean;
  lastFetchAt: number;
  lastSuccessAt: number;
  lastError: string;
  failCount: number;
  seeded: boolean;
  position: number;
  totalCount: number;
  unreadCount: number;
}
export type RssSourceInput = Partial<
  Omit<RssSource, 'lastFetchAt' | 'lastSuccessAt' | 'lastError' | 'failCount' | 'seeded' | 'totalCount' | 'unreadCount'>
> & { url: string };
export interface RssFeedItem {
  sourceId: string;
  guid: string;
  title: string;
  link: string;
  enclosureUrl: string;
  enclosureLength: number;
  pubDate: number;
  pubDateText: string;
  tags: string[];
  fetchedAt: number;
  status: RssItemStatus;
  taskUrl: string;
  episodeKey: string;
  reason: RssRejectReason;
}
export interface RssValidateRequest {
  url: string;
  mode?: RssSourceMode;
  itemPattern?: string;
  referer?: string;
  cookies?: string;
  userAgent?: string;
}
export interface RssValidateResult {
  url: string;
  mode: RssSourceMode;
  feedTitle: string;
  totalItems: number;
  sampleItems: RssFeedItem[];
}
export interface RssItemCounts {
  all: number;
  new: number;
  downloaded: number;
  ignored: number;
  filtered: number;
  duplicateEpisode: number;
  seedSkipped: number;
}
export interface RssItemsPage {
  sourceId: string;
  total: number;
  page: number;
  pageSize: number;
  counts: RssItemCounts;
  items: RssFeedItem[];
}
export interface RssActionResult {
  queued: number;
  skipped: number;
  updated: number;
  errors: { guid: string; message: string }[];
}
export interface RssRefreshResult {
  source: RssSource;
  added: number;
  autoQueued: number;
  totalItems: number;
}
