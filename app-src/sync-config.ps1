<#
.SYNOPSIS
    OneDrive 2-way sync — configuration (env-var based, synced via OneDrive).

.DESCRIPTION
    Dot-sourced by onedrive-sync-core.ps1. Every path uses $env:OneDriveConsumer /
    $env:USERPROFILE so the file is portable across machines and self-distributes.
    It is read-only at runtime and staged to the local app dir like the scripts.

    A machine that needs different roots can drop a local override at
    %LOCALAPPDATA%\onedrive-sync\sync-config.local.ps1 (dot-sourced after this file).
#>

if (-not $env:OneDriveConsumer) {
    throw "`$env:OneDriveConsumer is not set — is the personal OneDrive client running and signed in?"
}

# ── OneDrive parent folders whose .git-bearing children are projects ──────────
# Discovery recurses these (exclude-pruned) to find git repos at any depth.
# Local side is derived by the mirroring law: %USERPROFILE%\<same relpath>.
$ProjectParents = @(
    "$env:OneDriveConsumer\Projects"
)

# ── Local folders to watch for one-off projects mapped to an ARBITRARY OneDrive
#    location (folder-picker popup on first sight). Optional. ────────────────────
$WatchRoots = @(
    "$env:USERPROFILE\Code"
)

# ── Opt-in PLAIN (non-git) folders: explicit Local<->Dest, no git machinery ────
# Each entry: @{ Local = "<abs path>"; Dest = "<abs path under OneDrive>" }
$_claudeSlug = $env:USERPROFILE -replace '[:\\/.]', '-'
$PlainFolders = @(
    # @{ Local = "$env:USERPROFILE\3D\scripts"; Dest = "$env:OneDriveConsumer\3D printing\scripts" }
    @{ Local = "$env:USERPROFILE\.claude\projects\$_claudeSlug\memory"
       Dest  = "$env:OneDriveConsumer\claude-memory" }
)

# ── Sync rule for UNTRACKED files (tracked files ALWAYS sync) ──────────────────
# An untracked file syncs UNLESS git ignores it, EXCEPT the allow-list below.
# These static lists are a coarse, stable first cut applied on top.
$ExcludeDirs = @(
    # Build outputs / deps (NOT .git — that is synced)
    "node_modules"; ".pnpm-store"; ".yarn"
    "dist"; "build"; "out"; "target"; "bin"; "obj"
    ".next"; ".nuxt"; ".svelte-kit"; ".output"; "coverage"
    "__pycache__"; ".venv"; "venv"; ".pytest_cache"; ".mypy_cache"
    ".ruff_cache"; ".tox"; ".ipynb_checkpoints"
    "vendor"; ".cache"; ".parcel-cache"; ".idea"; ".vs"
    "logs"; "tmp"; "temp"
)

$ExcludeFiles = @(
    "*.pyc"; "*.pyo"; "*.pyd"
    ".DS_Store"; "Thumbs.db"; "desktop.ini"
    "*.swp"; "*.swo"; "*~"
    "*.tsbuildinfo"; "*.suo"; "*.user"; "*.eslintcache"
    "*.tmp"; "*.temp"; "*.log"
)

# Allow-list: untracked + gitignored files that SHOULD still sync (secrets/config).
# These end up in OneDrive's cloud — a conscious trade-off so pulled projects run.
$SyncAnywayList = @(
    ".env"; "*.env"; ".env.*"
    "*.local"
    "*.pem"; "*.key"; "*.p12"; "*.pfx"
)

# ── Versioning / safety / behaviour knobs ─────────────────────────────────────
$VersionRetentionDays = 30        # prune local archive older than this
$VersionMaxGB         = 5         # also prune oldest when archive exceeds this size
$MaxDeletePercent     = 25        # bisync brake against mass deletion
$IdleStabilitySeconds = 60        # OneDrive-idle gate window
$CompareMode          = 'modtime' # default compare; per-project override + adaptive
$NoiseThreshold       = 3         # spurious-conflict count -> recommend checksum
$QuietRunsToRevert    = 10        # checksum project quiet this many runs -> recommend modtime
$RetryMaxAttempts     = 4         # smart-retry of gated repos within a run
$RetryBackoff         = @(5, 10, 20)  # seconds between gated-repo retries
$RetryMaxWaitSeconds  = 120       # cap total backoff wait per repo before deferring
$DeferEscalateCycles  = 5         # escalate a repo deferred this many consecutive cycles
$RcloneTransfers      = 4
$ToolUpdateMode       = 'auto'    # 'auto' (copy newer source) | 'notify' (tray note)
$RunTimeBudget        = 1500      # seconds/run; over budget -> carry remaining repos
