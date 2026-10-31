// API types — mirror the Rust structs from monopsony-admin

export interface TailscaleState {
  available: boolean;
  serve_mode: boolean;
  funnel_mode: boolean;
  tailnet_name: string | null;
  self_ip: string | null;
  hostname: string | null;
}

export type AuthMethod = 'tailnet_auto_trust' | 'session' | 'none';

export interface AuthStatus {
  dashboard_enabled: boolean;
  auth_required: boolean;
  auth_method: AuthMethod;
  tailscale: TailscaleState;
  client_is_tailnet: boolean;
  credentials_configured: boolean;
}

export interface LoginRequest {
  username: string;
  password: string;
}

export interface LoginResponse {
  success: boolean;
  message: string | null;
  session_token: string | null;
}

export interface ServiceHealth {
  healthy: boolean;
  status: string;
  message: string | null;
}

export interface HealthResponse {
  status: string;
  services: Record<string, ServiceHealth>;
  uptime_seconds: number;
}

export interface StatsResponse {
  cache_dir: string;
  cache_exists: boolean;
  total_clients: number;
  total_packages: number;
  shadow_package_count: number;
}

export interface CacheEntry {
  url: string;
  content_type: string;
  content_length: number;
  cached_at: number;
  last_accessed: number;
  hit_count: number;
  sha256: string | null;
}

export interface CacheStats {
  cache_dir: string;
  total_entries: number;
  total_size_bytes: number;
  total_meta_files: number;
}

export interface ShadowPackage {
  id: string;
  name: string;
  version: string;
  source_type: string;
  source_url: string;
}

export interface RecipeValidationError {
  field: string;
  message: string;
}

export interface RecipeValidationResult {
  valid: boolean;
  errors: RecipeValidationError[];
}

export interface EstimateResponse {
  recipe_toml: string;
  detected: {
    name: string;
    version: string | null;
    arch: string;
    source_type: string;
    available_assets: string[];
  };
  warnings: string[];
}

export interface OsReleaseUpgrade {
  distro: string;
  from_version: string;
  to_version: string;
  ready: boolean;
  readiness_notes: string[];
}

// Sprint 29 Phase 4: rendered display-only upgrade guide (grounding D6).
export interface GuidedRecipeStep {
  order: number;
  group: string;
  action: string;
  blocking: boolean;
}

export interface GuidedRecipe {
  recipe_id: string;
  title: string;
  distro_label: string;
  downgrade_unsafe: boolean;
  config_paths: string[];
  steps: GuidedRecipeStep[];
}

export interface ReleaseGuideResponse {
  guide: GuidedRecipe | null;
  reason?: string;
  ready: boolean;
  readiness_notes: string[];
  distro_label: string;
}

export interface ClientUpdates {
  security: number;
  other: number;
  total: number;
  reported_at?: string;
  distro: string;
  distro_version: string;
  /** Phase 4: presentation label — PVE hosts show "Proxmox VE", not debian. */
  distro_label: string;
  releases: OsReleaseUpgrade[];
}

export interface ClientInfo {
  id: string;
  hostname: string;
  os: string;
  os_version?: string;
  registered_at: string;
  last_seen: string;
  /** Serial of the client's current certificate; null until a cert is issued. */
  cert_serial?: string | null;
  /** Sprint 29 B1: update counts + release rows; null until the client reports. */
  updates?: ClientUpdates | null;
}

/** One tracked client certificate (GET /clients/:id/certs, P4 cert status). */
export interface ClientCertInfo {
  serial: string;
  /** Owning client's UUID — present on list + audit rows. */
  client_id: string;
  /** Raw DB status: "active" | "revoked" | "expired". */
  status: string;
  /** Derived dashboard state — revoked wins over expired. */
  badge: 'valid' | 'expiring' | 'revoked' | 'expired';
  issued_at: string;
  expires_at: string;
  revoked_at: string | null;
  /** Whole days until expiry; null once expired. */
  days_to_expiry: number | null;
}

export interface ClientCertsResponse {
  certs: ClientCertInfo[];
  count: number;
}

// ─── Shadow Context / Assign Recipe ────────────────────────────────

export interface ShadowInventoryItem {
  name: string;
  version: string;
  manager: string;
}

export interface ScanFindingView {
  recipe_name: string;
  installed: boolean;
  detected_version: string;
  method: string;
}

export interface ShadowContext {
  inventory: ShadowInventoryItem[];
  scan_findings: ScanFindingView[];
  catalog: string[];
}

export interface AssignRecipeResponse {
  accepted: boolean;
  reason?: string;
}

export interface ConfigResponse {
  path: string;
  toml: string;
  error?: string;
}

// ─── Mirror Types ──────────────────────────────────────────────────

export interface MirrorConfig {
  distro: string;
  upstream_url: string;
  local_path: string;
  sync_enabled: boolean;
  sync_interval_secs: number;
  bandwidth_limit_bps: number | null;
  public: boolean;
  last_sync: string | null;
  sync_status: string;
  total_bytes: number;
}

export interface MirrorStatus {
  distro: string;
  sync_status: string;
  last_sync: string | null;
  total_bytes: number;
  sync_enabled: boolean;
  sync_interval_secs: number;
}

// ─── Tracker Types ─────────────────────────────────────────────────

export interface TrackerStatus {
  running: boolean;
  auth_mode: string;
  message?: string;
}

// ─── Pairing v2 approval (admin dashboard) ─────────────────────────

export type PendingPairStatus =
  | 'pending'
  | 'approved'
  | 'collected'
  | 'expired'
  | 'invalidated';

export interface PendingPairRequest {
  request_id: string;
  /** Self-reported by the client — forgeable, never trust for identity. */
  claimed_hostname: string;
  claimed_os: string;
  claimed_os_version: string | null;
  /** Observed peer IP — the trusted identity signal. */
  source_ip: string;
  created_at: number;
  expires_at: number;
  status: PendingPairStatus;
  pin_attempts: number;
}

export interface PairApproveResponse {
  approved: boolean;
  message?: string;
}

export interface PairAuditEntry {
  request_id: string;
  action: string;
  /** Known backend gap: currently always 'system' (session username not attributed). */
  approver: string;
  detail: string | null;
  created_at: number;
}

// ─── Enrollment keys (pairing-v2 P5) ────────────────────────────────

export interface EnrollmentKey {
  key_id: string;
  /** First 12 chars of the stored SHA-256 — identity hint, never the key. */
  key_hash_prefix: string;
  label: string;
  max_uses: number;
  use_count: number;
  created_at: number;
  expires_at: number | null;
  revoked_at: number | null;
}

export interface CreateEnrollmentKeyRequest {
  label: string;
  max_uses: number;
  /** Hours until expiry; omit/null = never expires. */
  expires_in_hours?: number | null;
}

export interface CreateEnrollmentKeyResponse {
  key_id: string;
  /** Plaintext key (mopk-...) — shown exactly once, never recoverable. */
  key: string;
  label: string;
  max_uses: number;
  expires_at: number | null;
}

// ─── Feature / Monetization Types ──────────────────────────────────

export interface FeatureFlags {
  shadow_enabled: boolean;
  storage_quota_enabled: boolean;
  storage_quota_gb: number;
  bandwidth_quota_enabled: boolean;
  bandwidth_quota_mbps: number;
  trust_mode: string;
  auto_blacklist_threshold: number;
}

export interface MonetizationConfig {
  public_metrics_enabled: boolean;
  lightning_address: string | null;
  bitcoin_address: string | null;
  openalias: string | null;
  bounty_address: string | null;
  bounty_critical_sats: number | null;
  bounty_high_sats: number | null;
  bounty_medium_sats: number | null;
  v4v_enabled: boolean;
  v4v_default_boost_sats: number | null;
}
