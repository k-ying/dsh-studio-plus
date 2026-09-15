//! Install the harness for the user instead of telling them to.
//!
//! The harness is an npm package, and asking someone to open a terminal and run
//! an install command is the point where a desktop app stops being one. So the
//! shell keeps its own copy under its data directory and installs it with the
//! same Node it already found — no global install, nothing on the user's PATH,
//! and no assumption that `npm` is reachable as a command.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use proc_guard::ProcessGuard;
use serde::{Deserialize, Serialize};
use tokio::io::BufReader;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::supervisor::Stream;
use crate::error::{Error, Result};

/// The package the harness ships as.
pub const PACKAGE: &str = "@deepseek-ai/dsh";

/// One coherent upstream release, never an npm moving tag.
///
/// Every official package in this release depends on the matching rc.2 family,
/// including the public `dsh-code-runtime-worker-thread` package. Pinning the
/// root keeps a newly installed machine from silently selecting an unrelated
/// release graph.
pub const VERSION: &str = "0.1.2-rc.1";
pub const PNPM_VERSION: &str = "11.7.0";
pub const PNPM_SPEC: &str = "pnpm@11.7.0";

/// The npm specifier that installs a given Harness release.
pub fn spec_for(version: &str) -> String {
    format!("{PACKAGE}@{version}")
}
const RUNTIME_SCHEMA: u8 = 3;
const INTEGRATION_PACKAGE: &str = "@moresyl/dsh-studio-integration";
const OFFICIAL_REGISTRY: &str = "https://registry.npmjs.org/";
const INSTALL_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
const INSTALL_TOTAL_TIMEOUT: Duration = Duration::from_secs(20 * 60);
const PIPE_DRAIN_TIMEOUT: Duration = Duration::from_secs(3);

const JOURNAL_VERSION: u8 = 1;
const RUNTIME_PACKAGE: &[u8] = include_bytes!("../../runtime-contract/package.json");
const RUNTIME_LOCK: &[u8] = include_bytes!("../../runtime-contract/package-lock.json");

/// Environment probes are allowed to recover a transaction left by a crashed
/// process, but must never mistake this process's live staging journal for a
/// crash. The command-layer guard prevents duplicate clicks; this lower-level
/// guard also covers Full/offline callers and every direct environment probe.
const MANAGED_RUNTIME_IDLE: u8 = 0;
const MANAGED_RUNTIME_INSTALLING: u8 = 1;
const MANAGED_RUNTIME_RECOVERING: u8 = 2;
static MANAGED_RUNTIME_ACTIVITY: AtomicU8 = AtomicU8::new(MANAGED_RUNTIME_IDLE);

struct ManagedInstallActivity;

impl ManagedInstallActivity {
    fn begin_install() -> Result<Self> {
        MANAGED_RUNTIME_ACTIVITY
            .compare_exchange(
                MANAGED_RUNTIME_IDLE,
                MANAGED_RUNTIME_INSTALLING,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .map_err(|_| Error::AlreadyInstalling)?;
        Ok(Self)
    }

    fn begin_recovery() -> Option<Self> {
        MANAGED_RUNTIME_ACTIVITY
            .compare_exchange(
                MANAGED_RUNTIME_IDLE,
                MANAGED_RUNTIME_RECOVERING,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .ok()
            .map(|_| Self)
    }
}

impl Drop for ManagedInstallActivity {
    fn drop(&mut self) {
        MANAGED_RUNTIME_ACTIVITY.store(MANAGED_RUNTIME_IDLE, Ordering::SeqCst);
    }
}
const INTEGRATION_MANIFEST: &[u8] =
    include_bytes!("../../runtime-contract/dsh-studio-integration/package.json");
const INTEGRATION_PATCH: &[u8] =
    include_bytes!("../../runtime-contract/dsh-studio-integration/cordis.patch.yml");
const INTEGRATION_NODE: &[u8] =
    include_bytes!("../../runtime-contract/dsh-studio-integration/lib/index.js");
const INTEGRATION_CLIENT: &[u8] =
    include_bytes!("../../runtime-contract/dsh-studio-integration/lib/client.js");
const INTEGRATION_RESOLVER: &[u8] =
    include_bytes!("../../runtime-contract/dsh-studio-integration/lib/runtime-resolver.cjs");

#[derive(Debug, Deserialize, Serialize)]
struct InstallJournal {
    schema: u8,
    package: String,
    version: String,
}

/// Everything needed to run one install.
#[derive(Clone, Debug)]
pub struct InstallPlan {
    /// Node runtime that will execute npm.
    pub node: PathBuf,
    /// npm's own entry script, run directly rather than through a shim.
    pub npm_cli: PathBuf,
    /// Directory that will hold `node_modules`.
    pub target: PathBuf,
    /// Package specifier, including any version.
    pub spec: String,
    /// The Harness release this plan installs; `VERSION` unless the channel
    /// pins another one.
    pub version: String,
}

impl InstallPlan {
    fn to_command(&self) -> Command {
        let mut command = Command::new(&self.node);
        command
            .arg(&self.npm_cli)
            .arg("install")
            .arg(&self.spec)
            .arg("--prefix")
            .arg(&self.target)
            // Nothing here is a project the user maintains, so npm's advice
            // about vulnerabilities and funding is noise in our log.
            .arg("--no-audit")
            .arg("--no-fund")
            // Lifecycle scripts include the native terminal dependencies. Keep
            // their stage and failure visible instead of leaving the UI silent.
            .arg("--foreground-scripts")
            // Without a TTY npm draws no progress bar; this is what keeps the
            // console moving during a download measured in hundreds of MB.
            .arg("--loglevel=http")
            .current_dir(&self.target)
            // Package lifecycle scripts expect to find `node` on PATH.
            .env("PATH", path_with_node(&self.node))
            .env("npm_config_update_notifier", "false")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        hide_console_window(&mut command);
        command
    }

    fn to_locked_command(&self) -> Command {
        let mut command = self.npm_command("ci");
        // Upstream rc.8 declares React 18 and ReactDOM 19 through separate
        // peer chains. The qualified lock records that exact working graph;
        // asking npm to solve those peers again defeats the lock and fails.
        command.arg("--legacy-peer-deps");
        hide_console_window(&mut command);
        command
    }

    /// `npm install` for a channel-pinned release, whose graph has no
    /// qualified lock and must be resolved from the manifest.
    fn to_install_command(&self) -> Command {
        let mut command = self.npm_command("install");
        command.arg("--legacy-peer-deps");
        hide_console_window(&mut command);
        command
    }

    fn npm_command(&self, verb: &str) -> Command {
        let mut command = Command::new(&self.node);
        command
            .arg(&self.npm_cli)
            .arg(verb)
            .arg("--prefix")
            .arg(&self.target)
            .arg("--no-audit")
            .arg("--no-fund")
            .arg("--foreground-scripts")
            .arg("--loglevel=http")
            // npm otherwise represents the bundled Studio integration as a
            // junction on Windows. The link can be observed as incomplete by
            // the verifier (and later breaks if its source is cleaned up), so
            // install the local file dependency as an ordinary directory.
            .arg("--install-links")
            // The lock was qualified against the public registry. npm otherwise
            // rewrites even locked tarball hosts to a user-configured mirror,
            // which can be incomplete or indefinitely stale.
            .arg(format!("--registry={OFFICIAL_REGISTRY}"))
            .arg("--fetch-retries=2")
            .arg("--fetch-timeout=60000")
            .current_dir(&self.target)
            .env("PATH", path_with_node(&self.node))
            .env("npm_config_update_notifier", "false")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        hide_console_window(&mut command);
        command
    }
}

fn hide_console_window(command: &mut Command) {
    #[cfg(windows)]
    {
        // npm and its lifecycle scripts report through the redirected pipes.
        // CREATE_NO_WINDOW prevents their console host from flashing over the
        // desktop shell while preserving that output for the Environment UI.
        command.creation_flags(0x0800_0000);
    }
    #[cfg(not(windows))]
    let _ = command;
}

/// Work out how to install `spec` with the given runtime.
pub fn plan(node: &Path, target: PathBuf, spec: String) -> Result<InstallPlan> {
    let npm_cli = npm_cli(node).ok_or(Error::NpmMissing)?;
    Ok(InstallPlan {
        node: node.to_path_buf(),
        npm_cli,
        target,
        version: spec
            .rsplit_once('@')
            .map(|(_, version)| version.to_string())
            .filter(|version| !version.is_empty())
            .unwrap_or_else(|| VERSION.to_string()),
        spec,
    })
}

/// Run the install, reporting every line npm produces.
pub async fn run<R>(plan: &InstallPlan, report: R) -> Result<()>
where
    R: Fn(Stream, String) + Clone + Send + 'static,
{
    std::fs::create_dir_all(&plan.target).map_err(|cause| {
        Error::Install(format!(
            "could not create {}: {cause}",
            plan.target.display()
        ))
    })?;

    run_command(plan.to_command(), report, "npm install").await
}

/// Install into an isolated sibling, verify it, then promote it in one rename.
///
/// The journal deliberately has no changing phase field. Recovery derives the
/// truth from the three directories, so a crash can never leave a phase that
/// claims a rename happened when the filesystem says otherwise.
pub async fn run_transactional<R>(plan: &InstallPlan, report: R) -> Result<()>
where
    R: Fn(Stream, String) + Clone + Send + 'static,
{
    let expected = spec_for(&super::channel::selected());
    if plan.spec != expected {
        return Err(Error::Install(
            "managed runtime install did not use the qualified Harness contract".into(),
        ));
    }
    let _activity = ManagedInstallActivity::begin_install()?;
    recover_managed_install_inner()?;

    let live = &plan.target;
    let staging = crate::paths::harness_staging_dir();
    let backup = crate::paths::harness_backup_dir();
    let journal = crate::paths::harness_install_journal();

    remove_dir_if_exists(&staging)?;
    remove_dir_if_exists(&backup)?;
    write_journal(&journal, &plan.version)?;

    let staged_plan = InstallPlan {
        target: staging.clone(),
        ..plan.clone()
    };
    if let Err(failure) = run_locked(&staged_plan, report).await {
        let _ = remove_dir_if_exists(&staging);
        let _ = std::fs::remove_file(&journal);
        return Err(failure);
    }

    require_expected_runtime(&staging)?;

    promote(live, &staging, &backup, &journal)
}

async fn run_locked<R>(plan: &InstallPlan, report: R) -> Result<()>
where
    R: Fn(Stream, String) + Clone + Send + 'static,
{
    std::fs::create_dir_all(&plan.target).map_err(|cause| {
        Error::Install(format!(
            "could not create {}: {cause}",
            plan.target.display()
        ))
    })?;
    std::fs::write(
        plan.target.join("package.json"),
        runtime_manifest_for(&plan.version)?,
    )
    .map_err(|cause| Error::Install(format!("could not stage the runtime manifest: {cause}")))?;
    // The embedded lock describes exactly the built-in release graph. A
    // channel-pinned release resolves its own graph instead.
    if plan.version == VERSION {
        std::fs::write(plan.target.join("package-lock.json"), RUNTIME_LOCK)
            .map_err(|cause| Error::Install(format!("could not stage the runtime lock: {cause}")))?;
    }
    stage_integration(&plan.target)?;

    if plan.version == VERSION {
        run_command(plan.to_locked_command(), report, "npm ci").await?;
    } else {
        run_command(plan.to_install_command(), report, "npm install").await?;
    }
    qualify_runtime(&plan.target)?;
    Ok(())
}

/// The runtime manifest aimed at `version`: the `@deepseek-ai/*` packages
/// pinned to the built-in release follow the Harness in lockstep and move
/// with it; anything at its own version (cordis-plugin-group, say) keeps it.
fn runtime_manifest_for(version: &str) -> Result<Vec<u8>> {
    let mut manifest: serde_json::Value = serde_json::from_slice(RUNTIME_PACKAGE)
        .map_err(|cause| Error::Install(format!("the runtime manifest template is unreadable: {cause}")))?;
    for section in ["dependencies", "devDependencies"] {
        if let Some(table) = manifest.get_mut(section).and_then(|table| table.as_object_mut()) {
            for (name, pinned) in table.iter_mut() {
                if name.starts_with("@deepseek-ai/") && pinned.as_str() == Some(VERSION) {
                    *pinned = serde_json::Value::String(version.to_string());
                }
            }
        }
    }
    serde_json::to_vec_pretty(&manifest)
        .map_err(|cause| Error::Install(format!("could not encode the runtime manifest: {cause}")))
}

async fn run_command<R>(command: Command, report: R, label: &'static str) -> Result<()>
where
    R: Fn(Stream, String) + Clone + Send + 'static,
{
    run_command_with_limits(
        command,
        report,
        label,
        INSTALL_IDLE_TIMEOUT,
        INSTALL_TOTAL_TIMEOUT,
        PIPE_DRAIN_TIMEOUT,
    )
    .await
}

async fn run_command_with_limits<R>(
    mut command: Command,
    report: R,
    label: &'static str,
    idle_timeout: Duration,
    total_timeout: Duration,
    pipe_drain_timeout: Duration,
) -> Result<()>
where
    R: Fn(Stream, String) + Clone + Send + 'static,
{
    // Installation gets its own job/process group. A timeout can therefore
    // reclaim npm's whole lifecycle tree without terminating a running Harness
    // owned by the supervisor's independent guard.
    let guard = ProcessGuard::new().map_err(Error::Spawn)?;
    let mut child = guard.spawn(&mut command).map_err(Error::Spawn)?;
    let pid = child.id();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (Some(stdout), Some(stderr)) = (stdout, stderr) else {
        let _ = child.kill().await;
        let _ = child.wait().await;
        if let Some(pid) = pid {
            let _ = guard.finish(pid);
        }
        return Err(Error::Install(format!(
            "{label} did not provide its diagnostic pipes"
        )));
    };
    let (activity, mut observed) = mpsc::channel(1);
    let mut out = tokio::spawn(forward(
        stdout,
        Stream::Stdout,
        report.clone(),
        activity.clone(),
    ));
    let mut err = tokio::spawn(forward(stderr, Stream::Stderr, report, activity.clone()));
    drop(activity);

    let idle = tokio::time::sleep(idle_timeout);
    let total = tokio::time::sleep(total_timeout);
    tokio::pin!(idle, total);
    let mut observing = true;
    let status = loop {
        tokio::select! {
            biased;
            result = child.wait() => {
                if let Some(pid) = pid {
                    if let Err(cause) = guard.finish(pid) {
                        out.abort();
                        err.abort();
                        return Err(Error::Install(format!(
                            "{label} process tree could not be reclaimed: {cause}"
                        )));
                    }
                }
                break result.map_err(|cause| {
                    Error::Install(format!("{label} could not be waited on: {cause}"))
                })?;
            }
            activity = observed.recv(), if observing => {
                match activity {
                    Some(()) => idle.as_mut().reset(Instant::now() + idle_timeout),
                    None => observing = false,
                }
            }
            _ = &mut idle => {
                let _ = guard.terminate_all();
                let _ = child.wait().await;
                if let Some(pid) = pid {
                    let _ = guard.finish(pid);
                }
                out.abort();
                err.abort();
                return Err(Error::Install(format!(
                    "{label} produced no output for 120 seconds and was stopped; retry on a working connection or use Full / Offline"
                )));
            }
            _ = &mut total => {
                let _ = guard.terminate_all();
                let _ = child.wait().await;
                if let Some(pid) = pid {
                    let _ = guard.finish(pid);
                }
                out.abort();
                err.abort();
                return Err(Error::Install(format!(
                    "{label} exceeded the 20 minute safety limit and was stopped; retry on a working connection or use Full / Offline"
                )));
            }
        }
    };

    // A lifecycle-script descendant can inherit the output handles after npm
    // itself exits on Windows. Drain normal output briefly, but never turn a
    // successful or failed npm exit into a permanently spinning UI.
    if tokio::time::timeout(pipe_drain_timeout, async {
        let _ = tokio::join!(&mut out, &mut err);
    })
    .await
    .is_err()
    {
        out.abort();
        err.abort();
    }

    if !status.success() {
        return Err(Error::Install(format!("{label} exited with {status}")));
    }
    Ok(())
}

fn stage_integration(target: &Path) -> Result<()> {
    let root = target.join("dsh-studio-integration");
    std::fs::create_dir_all(root.join("lib"))
        .and_then(|_| std::fs::write(root.join("package.json"), INTEGRATION_MANIFEST))
        .and_then(|_| std::fs::write(root.join("cordis.patch.yml"), INTEGRATION_PATCH))
        .and_then(|_| std::fs::write(root.join("lib/index.js"), INTEGRATION_NODE))
        .and_then(|_| std::fs::write(root.join("lib/client.js"), INTEGRATION_CLIENT))
        .and_then(|_| std::fs::write(root.join("lib/runtime-resolver.cjs"), INTEGRATION_RESOLVER))
        .map_err(|cause| Error::Install(format!("could not stage the Studio integration: {cause}")))
}

/// Materialize launch support added by a Studio upgrade into an otherwise
/// compatible managed runtime. The integration is Studio-owned, so this does
/// not mutate the upstream Harness or any user Profile.
pub fn ensure_runtime_resolver(target: &Path) -> Result<PathBuf> {
    let integration = target.join("node_modules/@moresyl/dsh-studio-integration");
    for directory in [&integration, &integration.join("lib")] {
        let metadata = std::fs::symlink_metadata(directory).map_err(|cause| {
            Error::Install(format!(
                "the managed Studio integration is incomplete at {}: {cause}; use Repair in Environment",
                directory.display()
            ))
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(Error::Install(format!(
                "the managed Studio integration has an unsafe directory at {}; use Repair in Environment",
                directory.display()
            )));
        }
    }
    let entry = integration.join("lib/index.js");
    let entry_metadata = std::fs::symlink_metadata(&entry).map_err(|_| {
        Error::Install(
            "the managed Studio integration is missing; use Repair in Environment".into(),
        )
    })?;
    if entry_metadata.file_type().is_symlink() || !entry_metadata.is_file() {
        return Err(Error::Install(
            "the managed Studio integration entry is unsafe; use Repair in Environment".into(),
        ));
    }
    let resolver = integration.join("lib/runtime-resolver.cjs");
    if let Ok(metadata) = std::fs::symlink_metadata(&resolver) {
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(Error::Install(format!(
                "the managed runtime resolver has an unsafe file at {}",
                resolver.display()
            )));
        }
        if crate::bounded_file::read(&resolver, crate::bounded_file::CONTROL_BYTES)
            .is_ok_and(|body| body == INTEGRATION_RESOLVER)
        {
            return Ok(resolver);
        }
    }
    crate::atomic::write(&resolver, INTEGRATION_RESOLVER).map_err(|cause| {
        Error::Install(format!(
            "the managed runtime resolver could not be updated at {}: {cause}",
            resolver.display()
        ))
    })?;
    Ok(resolver)
}

/// One textual seam the runtime qualification rewrites.
struct Patch {
    from: &'static str,
    to: &'static str,
    label: &'static str,
}

/// The directory picker's native-fork enhancements. The whole group applies
/// or none of it does: half an enhanced picker is worse than the stock one.
const PICKER_PATCHES: &[Patch] = &[
    Patch {
        from: "function DirectoryBrowser({ open, listDirectory, createDirectory, onOpen, onClose, busy, t }) {",
        to: "function DirectoryBrowser({ open, listDirectory, createDirectory, pickNativeDirectory, validateDirectory, onOpen, onClose, busy, t }) {",
        label: "directory browser arguments",
    },
    Patch {
        from: "\t\t\tconst [createError, setCreateError] = (0, react.useState)(null);",
        to: "\t\t\tconst [createError, setCreateError] = (0, react.useState)(null);\n\t\t\tconst [nativePicking, setNativePicking] = (0, react.useState)(false);\n\t\t\tconst [validatingDirectory, setValidatingDirectory] = (0, react.useState)(false);",
        label: "directory browser native state",
    },
    Patch {
        from: "\t\t\tconst parentInert = busy || folderDraft !== null;\n\t\t\tconst draftPending = pathDraft !== null;",
        to: "\t\t\tconst parentInert = busy || folderDraft !== null || nativePicking || validatingDirectory;\n\t\t\tconst openDirectory = (path) => {\n\t\t\t\tif (validateDirectory === void 0) {\n\t\t\t\t\tonOpen(path);\n\t\t\t\t\treturn;\n\t\t\t\t}\n\t\t\t\tsetError(null);\n\t\t\t\tsetValidatingDirectory(true);\n\t\t\t\tvalidateDirectory(path).then((allowed) => {\n\t\t\t\t\tsetValidatingDirectory(false);\n\t\t\t\t\tif (allowed) onOpen(path);\n\t\t\t\t}, (reason) => {\n\t\t\t\t\tsetValidatingDirectory(false);\n\t\t\t\t\tsetError(failureText(reason));\n\t\t\t\t});\n\t\t\t};\n\t\t\tconst pickFromSystem = () => {\n\t\t\t\tif (pickNativeDirectory === void 0) return;\n\t\t\t\tsetError(null);\n\t\t\t\tsetNativePicking(true);\n\t\t\t\tpickNativeDirectory().then((path) => {\n\t\t\t\t\tsetNativePicking(false);\n\t\t\t\t\tif (path !== null) openDirectory(path);\n\t\t\t\t}, (reason) => {\n\t\t\t\t\tsetNativePicking(false);\n\t\t\t\t\tsetError(failureText(reason));\n\t\t\t\t});\n\t\t\t};\n\t\t\tconst draftPending = pathDraft !== null;",
        label: "directory browser native actions",
    },
    Patch {
        from: "\t\t\t\t\tif (folderDraft === null && !busy) onClose();",
        to: "\t\t\t\t\tif (!parentInert) onClose();",
        label: "directory browser close guard",
    },
    Patch {
        from: "\t\t\t\t\t\t\t\t(0, react_jsx_runtime.jsxs)(\"button\", {\n\t\t\t\t\t\t\t\t\ttype: \"button\",\n\t\t\t\t\t\t\t\t\tclassName: clsx(DirectoryBrowser_module_css_default.showHiddenToggle, showHidden && DirectoryBrowser_module_css_default.showHiddenToggleActive),",
        to: "\t\t\t\t\t\t\t\tpickNativeDirectory !== void 0 && (0, react_jsx_runtime.jsx)(_deepseek_ai_dsh_client_ui_primitives.Button, {\n\t\t\t\t\t\t\t\t\tvariant: \"outline\",\n\t\t\t\t\t\t\t\t\ticon: (0, react_jsx_runtime.jsx)(_deepseek_ai_dsh_client_ui_primitives.IconFolderOpen16, { size: 16 }),\n\t\t\t\t\t\t\t\t\tdisabled: parentInert,\n\t\t\t\t\t\t\t\t\tonClick: pickFromSystem,\n\t\t\t\t\t\t\t\t\tchildren: t(\"browser.nativePicker\")\n\t\t\t\t\t\t\t\t}),\n\t\t\t\t\t\t\t\t(0, react_jsx_runtime.jsxs)(\"button\", {\n\t\t\t\t\t\t\t\t\ttype: \"button\",\n\t\t\t\t\t\t\t\t\tclassName: clsx(DirectoryBrowser_module_css_default.showHiddenToggle, showHidden && DirectoryBrowser_module_css_default.showHiddenToggleActive),",
        label: "directory browser native button",
    },
    Patch {
        from: "if (targetPath !== null) onOpen(targetPath);",
        to: "if (targetPath !== null) openDirectory(targetPath);",
        label: "directory browser open validation",
    },
    Patch {
        from: "\t\t\t\tcreateDirectory: props.createDirectory,\n\t\t\t\tt: props.t,",
        to: "\t\t\t\tcreateDirectory: props.createDirectory,\n\t\t\t\tpickNativeDirectory: props.pickNativeDirectory,\n\t\t\t\tvalidateDirectory: props.validateDirectory,\n\t\t\t\tt: props.t,",
        label: "browse flow native properties",
    },
    Patch {
        from: "\"browser.showHidden\": \"显示隐藏文件\"",
        to: "\"browser.showHidden\": \"显示隐藏文件\",\n\t\t\t\t\t\"browser.nativePicker\": \"使用系统选择文件夹\"",
        label: "Chinese directory picker copy",
    },
    Patch {
        from: "\"browser.showHidden\": \"Show hidden files\"",
        to: "\"browser.showHidden\": \"Show hidden files\",\n\t\t\t\t\t\"browser.nativePicker\": \"Choose with system dialog\"",
        label: "English directory picker copy",
    },
    Patch {
        from: "\t\t\t\tcreateDirectory: (path, name) => ctx.uiWorkspace.createDirectory(path, name),\n\t\t\t\tt: ctx.locale.bind(LOCALE_NS)",
        to: "\t\t\t\tcreateDirectory: (path, name) => ctx.uiWorkspace.createDirectory(path, name),\n\t\t\t\tpickNativeDirectory: typeof window.__DSH_DESKTOP_PICK_DIRECTORY__ === \"function\" ? () => window.__DSH_DESKTOP_PICK_DIRECTORY__() : void 0,\n\t\t\t\tvalidateDirectory: typeof window.__DSH_DESKTOP_VALIDATE_DIRECTORY__ === \"function\" ? (path) => window.__DSH_DESKTOP_VALIDATE_DIRECTORY__(path) : void 0,\n\t\t\t\tt: ctx.locale.bind(LOCALE_NS)",
        label: "directory picker desktop injection",
    },
];

/// The browser-session exemption every Studio frame depends on. The Harness
/// 0.1.2 fence mints a SameSite cookie the shell's frame can never present
/// back, so a Studio-launched harness (DSH_DESKTOP is set by the supervisor)
/// treats every loopback caller as authenticated — exactly the exposure 0.1.1
/// shipped with. A dsh launched from a terminal has no such variable and
/// keeps the fence. Required: without it the frame gets a 401.
const CONNECTION_PATCHES: &[Patch] = &[Patch {
    from: "\tisAuthenticated(request) {\n\t\tconst authority = requestAuthority(request.headers);",
    to: "\tisAuthenticated(request) {\n\t\tif (process.env.DSH_DESKTOP !== void 0) return true;\n\t\tconst authority = requestAuthority(request.headers);",
    label: "browser session desktop exemption",
}];

/// Whether every seam of a patch group is present (or already applied) exactly
/// once — the preflight question, answered without touching anything.
fn group_applies(body: &str, patches: &[Patch]) -> bool {
    patches.iter().all(|patch| {
        body.matches(patch.to).count() == 1 || body.matches(patch.from).count() == 1
    })
}

fn apply_group(body: String, patches: &[Patch]) -> Result<String> {
    let mut body = body;
    for patch in patches {
        body = replace_once(body, patch.from, patch.to, patch.label)?;
    }
    Ok(body)
}

/// How a Harness release answers Studio's patches before anyone installs it.
#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Preflight {
    pub version: String,
    /// The required browser-session exemption applies.
    pub connection: bool,
    /// The optional directory-picker enhancement group applies.
    pub picker: bool,
}

/// Answer preflight from already-fetched package bodies — pure, and the exact
/// same data the installer itself applies.
pub fn preflight_bodies(version: &str, connection_body: &str, picker_body: &str) -> Preflight {
    Preflight {
        version: version.to_string(),
        connection: group_applies(connection_body, CONNECTION_PATCHES),
        picker: group_applies(picker_body, PICKER_PATCHES),
    }
}

/// Download just the two patched packages of `version` and check their seams,
/// so the settings panel can say what a switch would look like before the
/// installer ever runs.
pub async fn preflight(node: &Path, npm_cli: &Path, version: &str) -> Result<Preflight> {
    let scratch = std::env::temp_dir().join(format!("dsh-studio-preflight-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch)
        .map_err(|cause| Error::Install(format!("could not stage the preflight probe: {cause}")))?;
    let probe = async {
        let connection_body = pack_member(
            node,
            npm_cli,
            &scratch,
            &format!("@deepseek-ai/dsh-client-connection@{version}"),
            "lib/index.js",
        )
        .await?;
        let picker_body = pack_member(
            node,
            npm_cli,
            &scratch,
            &format!("@deepseek-ai/dsh-client-ui-directory-picker-browse@{version}"),
            "lib/client.js",
        )
        .await?;
        Ok(preflight_bodies(version, &connection_body, &picker_body))
    };
    let outcome = probe.await;
    let _ = std::fs::remove_dir_all(&scratch);
    outcome
}

/// `npm pack` one package into `scratch` and read a single member out of the
/// tarball, without unpacking anything else.
async fn pack_member(
    node: &Path,
    npm_cli: &Path,
    scratch: &Path,
    spec: &str,
    member: &str,
) -> Result<String> {
    let mut command = Command::new(node);
    command
        .arg(npm_cli)
        .arg("pack")
        .arg(spec)
        .arg("--pack-destination")
        .arg(scratch)
        .arg(format!("--registry={OFFICIAL_REGISTRY}"))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    hide_console_window(&mut command);
    let finished = tokio::time::timeout(Duration::from_secs(90), command.output())
        .await
        .map_err(|_| Error::Network(format!("npm pack {spec} timed out")))?
        .map_err(|cause| Error::Network(format!("could not run npm pack: {cause}")))?;
    if !finished.status.success() {
        return Err(Error::Network(format!(
            "npm pack {spec} failed: {}",
            String::from_utf8_lossy(&finished.stderr).trim()
        )));
    }
    let tarball = std::fs::read_dir(scratch)
        .map_err(|cause| Error::Network(format!("the pack destination vanished: {cause}")))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .find(|path| path.extension().is_some_and(|ext| ext == "tgz"))
        .ok_or_else(|| Error::Network(format!("npm pack {spec} left no tarball")))?;
    let member_path = format!("package/{member}");
    let file = std::fs::File::open(&tarball)
        .map_err(|cause| Error::Network(format!("the packed tarball could not be read: {cause}")))?;
    let decoded = flate2::read::GzDecoder::new(std::io::BufReader::new(file));
    let mut archive = tar::Archive::new(decoded);
    let mut entries = archive
        .entries()
        .map_err(|cause| Error::Network(format!("the packed tarball is unreadable: {cause}")))?;
    for entry in entries.by_ref() {
        let mut entry =
            entry.map_err(|cause| Error::Network(format!("a packed member is unreadable: {cause}")))?;
        let matches = entry
            .path()
            .map(|path| path == std::path::Path::new(&member_path))
            .unwrap_or(false);
        if matches {
            let mut body = String::new();
            std::io::Read::read_to_string(&mut entry, &mut body).map_err(|cause| {
                Error::Network(format!("the packed member could not be read: {cause}"))
            })?;
            std::fs::remove_file(&tarball).ok();
            return Ok(body);
        }
    }
    Err(Error::Network(format!(
        "the packed {spec} has no {member}"
    )))
}

fn qualify_runtime(target: &Path) -> Result<()> {
    let mut degraded: Vec<&str> = Vec::new();

    let client = target
        .join("node_modules/@deepseek-ai/dsh-client-ui-directory-picker-browse/lib/client.js");
    let body = crate::bounded_file::read_string(&client, crate::bounded_file::CONTROL_BYTES)
        .map_err(|cause| {
            Error::Install(format!(
                "the qualified directory picker could not be read safely: {cause}"
            ))
        })?;
    if group_applies(&body, PICKER_PATCHES) {
        std::fs::write(&client, apply_group(body, PICKER_PATCHES)?).map_err(|cause| {
            Error::Install(format!(
                "the qualified directory picker could not be written: {cause}"
            ))
        })?;
    } else {
        // Optional enhancement: the stock picker still works, and the marker
        // records what the frame is missing so the contract tolerates it.
        degraded.push("directory picker");
    }

    let connection = target.join("node_modules/@deepseek-ai/dsh-client-connection/lib/index.js");
    let connection_body = crate::bounded_file::read_string(&connection, crate::bounded_file::CONTROL_BYTES)
        .map_err(|cause| {
            Error::Install(format!(
                "the qualified client connection could not be read safely: {cause}"
            ))
        })?;
    if !group_applies(&connection_body, CONNECTION_PATCHES) {
        return Err(Error::Install(
            "this Harness release moved the browser-session fence, so Studio cannot qualify it — pin a supported dsh release".into(),
        ));
    }
    std::fs::write(&connection, apply_group(connection_body, CONNECTION_PATCHES)?).map_err(
        |cause| {
            Error::Install(format!(
                "the qualified client connection could not be written: {cause}"
            ))
        },
    )?;

    let marker = serde_json::json!({ "schema": RUNTIME_SCHEMA, "degraded": degraded });
    std::fs::write(
        target.join("dsh-studio-runtime.json"),
        format!("{}\n", serde_json::to_string_pretty(&marker).unwrap_or_default()),
    )
    .map_err(|cause| Error::Install(format!("could not mark the runtime contract: {cause}")))
}

fn replace_once(body: String, from: &str, to: &str, label: &str) -> Result<String> {
    // Qualification runs after every managed install and may also inspect an
    // already-qualified runtime recovered from an interrupted promotion. Some
    // replacements deliberately retain `from` inside `to`, so checking the
    // completed replacement first is what makes the operation truly idempotent.
    if body.matches(to).count() == 1 {
        return Ok(body);
    }
    if body.matches(from).count() != 1 {
        return Err(Error::Install(format!(
            "the qualified Harness no longer has the expected {label} seam"
        )));
    }
    Ok(body.replacen(from, to, 1))
}

/// Restore a Full package's pre-resolved dependency closure without npm.
pub fn run_bundled(artifact: &crate::offline::Artifact) -> Result<()> {
    let _activity = ManagedInstallActivity::begin_install()?;
    recover_managed_install_inner()?;

    let live = crate::paths::harness_dir();
    let staging = crate::paths::harness_staging_dir();
    let backup = crate::paths::harness_backup_dir();
    let journal = crate::paths::harness_install_journal();
    remove_dir_if_exists(&staging)?;
    remove_dir_if_exists(&backup)?;
    write_journal(&journal, VERSION)?;

    let prepared = (|| {
        let file = crate::offline::verified_file(artifact)?;
        std::fs::create_dir_all(&staging).map_err(|cause| {
            Error::Install(format!(
                "could not create the offline install directory: {cause}"
            ))
        })?;
        let decoded = flate2::read::GzDecoder::new(std::io::BufReader::new(file));
        tar::Archive::new(decoded)
            .unpack(&staging)
            .map_err(|cause| {
                Error::Install(format!(
                    "the offline Harness archive could not be unpacked: {cause}"
                ))
            })?;
        require_expected_runtime(&staging)
    })();
    if let Err(failure) = prepared {
        let _ = remove_dir_if_exists(&staging);
        let _ = std::fs::remove_file(&journal);
        return Err(failure);
    }

    promote(&live, &staging, &backup, &journal)
}

fn promote(live: &Path, staging: &Path, backup: &Path, journal: &Path) -> Result<()> {
    if live.exists() {
        std::fs::rename(live, backup).map_err(|cause| {
            Error::Install(format!(
                "could not preserve the current Harness runtime before upgrading: {cause}"
            ))
        })?;
    }

    if let Err(cause) = std::fs::rename(staging, live) {
        if backup.exists() && !live.exists() {
            let _ = std::fs::rename(backup, live);
        }
        return Err(Error::Install(format!(
            "could not activate the verified Harness runtime: {cause}"
        )));
    }

    if let Err(failure) = require_expected_runtime(live) {
        let _ = remove_dir_if_exists(live);
        if backup.exists() {
            let _ = std::fs::rename(backup, live);
        }
        return Err(failure);
    }

    remove_dir_if_exists(backup)?;
    std::fs::remove_file(journal).map_err(|cause| {
        Error::Install(format!(
            "the Harness runtime is ready but its install journal could not be cleared: {cause}"
        ))
    })?;
    Ok(())
}

/// Repair an install interrupted before, during, or after the directory swap.
///
/// Returns `true` when a journal was present. It is safe to call on every
/// environment probe; without the marker it performs no filesystem writes.
pub fn recover_managed_install() -> Result<bool> {
    let Some(_activity) = ManagedInstallActivity::begin_recovery() else {
        return Ok(false);
    };
    recover_managed_install_inner()
}

fn recover_managed_install_inner() -> Result<bool> {
    let journal = crate::paths::harness_install_journal();
    if !journal.exists() {
        return Ok(false);
    }
    let state = read_journal(&journal)?;

    let live = crate::paths::harness_dir();
    let staging = crate::paths::harness_staging_dir();
    let backup = crate::paths::harness_backup_dir();

    if runtime_complete(&live) {
        remove_dir_if_exists(&staging)?;
        remove_dir_if_exists(&backup)?;
    } else if runtime_complete(&backup) {
        remove_dir_if_exists(&live)?;
        std::fs::rename(&backup, &live).map_err(|cause| {
            Error::Install(format!(
                "could not restore the previous Harness runtime: {cause}"
            ))
        })?;
        remove_dir_if_exists(&staging)?;
    } else if runtime_version(&staging).as_deref() == Some(state.version.as_str()) {
        remove_dir_if_exists(&live)?;
        std::fs::rename(&staging, &live).map_err(|cause| {
            Error::Install(format!(
                "could not finish activating the Harness runtime: {cause}"
            ))
        })?;
        remove_dir_if_exists(&backup)?;
    } else {
        // Nothing complete existed before or after the interruption. Keeping a
        // marker here would make the Repair button fail on every attempt.
        remove_dir_if_exists(&live)?;
        remove_dir_if_exists(&staging)?;
        remove_dir_if_exists(&backup)?;
    }

    std::fs::remove_file(&journal).map_err(|cause| {
        Error::Install(format!(
            "could not clear the recovered install journal: {cause}"
        ))
    })?;
    Ok(true)
}

/// Version recorded by a complete managed runtime.
pub fn runtime_version(target: &Path) -> Option<String> {
    let manifest = target
        .join("node_modules")
        .join("@deepseek-ai")
        .join("dsh")
        .join("package.json");
    let raw =
        crate::bounded_file::read_string(&manifest, crate::bounded_file::CONTROL_BYTES).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let version = parsed.get("version")?.as_str()?.trim();
    (!version.is_empty()).then(|| version.to_string())
}

/// The release the installed runtime actually is, falling back to what the
/// channel would install. Plugin compatibility answers about what runs, not
/// about what the build pinned.
pub fn current_version() -> String {
    runtime_version(&crate::paths::harness_dir()).unwrap_or_else(super::channel::selected)
}

/// Whether the installed runtime is exactly the family this application tested.
pub fn runtime_compatible(target: &Path) -> bool {
    runtime_contract_failures(target).is_empty()
}

fn runtime_complete(target: &Path) -> bool {
    runtime_version(target).is_some() && entry(target).is_file()
}

fn entry(target: &Path) -> PathBuf {
    target
        .join("node_modules")
        .join("@deepseek-ai")
        .join("dsh")
        .join("lib")
        .join("bin.js")
}

pub fn pnpm_version(target: &Path) -> Option<String> {
    let manifest = target.join("node_modules/pnpm/package.json");
    let raw =
        crate::bounded_file::read_string(&manifest, crate::bounded_file::CONTROL_BYTES).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&raw).ok()?;
    parsed.get("version")?.as_str().map(str::to_string)
}

fn pnpm_entry(target: &Path) -> PathBuf {
    target.join("node_modules/pnpm/bin/pnpm.cjs")
}

fn integration_entry(target: &Path) -> PathBuf {
    target.join("node_modules/@moresyl/dsh-studio-integration/lib/client.js")
}

/// The contract marker left by qualification: its schema and the optional
/// enhancement groups that release could not take.
fn runtime_marker(target: &Path) -> Option<(u8, Vec<String>)> {
    let raw = crate::bounded_file::read_string(
        &target.join("dsh-studio-runtime.json"),
        crate::bounded_file::CONTROL_BYTES,
    )
    .ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let schema = value.get("schema")?.as_u64()?.try_into().ok()?;
    let degraded = value
        .get("degraded")
        .and_then(|degraded| degraded.as_array())
        .map(|degraded| {
            degraded
                .iter()
                .filter_map(|entry| entry.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    Some((schema, degraded))
}

fn qualified_picker(target: &Path) -> bool {
    crate::bounded_file::read_string(
        &target
            .join("node_modules/@deepseek-ai/dsh-client-ui-directory-picker-browse/lib/client.js"),
        crate::bounded_file::CONTROL_BYTES,
    )
    .is_ok_and(|body| {
        body.contains("__DSH_DESKTOP_PICK_DIRECTORY__")
            && body.contains("__DSH_DESKTOP_VALIDATE_DIRECTORY__")
    })
}

fn qualified_connection(target: &Path) -> bool {
    crate::bounded_file::read_string(
        &target.join("node_modules/@deepseek-ai/dsh-client-connection/lib/index.js"),
        crate::bounded_file::CONTROL_BYTES,
    )
    .is_ok_and(|body| body.contains("process.env.DSH_DESKTOP !== void 0"))
}

fn require_expected_runtime(target: &Path) -> Result<()> {
    let actual = runtime_version(target).unwrap_or_else(|| "missing".to_string());
    let actual_pnpm = pnpm_version(target).unwrap_or_else(|| "missing".to_string());
    let expected = super::channel::selected();
    let failures = runtime_contract_failures(target);
    if !failures.is_empty() {
        return Err(Error::Install(format!(
            "npm finished but the verified runtime is not Studio contract {RUNTIME_SCHEMA} with {PACKAGE}@{expected}, {INTEGRATION_PACKAGE}, and pnpm {PNPM_VERSION} (found {actual} with pnpm {actual_pnpm}; failed: {})",
            failures.join(", ")
        )));
    }
    Ok(())
}

fn runtime_contract_failures(target: &Path) -> Vec<&'static str> {
    let mut failures = Vec::new();
    if runtime_version(target).as_deref() != Some(super::channel::selected().as_str()) {
        failures.push("Harness version");
    }
    if !entry(target).is_file() {
        failures.push("Harness entry point");
    }
    if pnpm_version(target).as_deref() != Some(PNPM_VERSION) {
        failures.push("pnpm version");
    }
    if !pnpm_entry(target).is_file() {
        failures.push("pnpm entry point");
    }
    let marker = runtime_marker(target);
    if marker.as_ref().map(|(schema, _)| *schema) != Some(RUNTIME_SCHEMA) {
        failures.push("runtime marker");
    }
    if !integration_entry(target).is_file() {
        failures.push("Studio integration");
    }
    let picker_excused = marker
        .map(|(_, degraded)| degraded.iter().any(|group| group == "directory picker"))
        .unwrap_or(false);
    if !picker_excused && !qualified_picker(target) {
        failures.push("qualified directory picker");
    }
    if !qualified_connection(target) {
        failures.push("qualified client connection");
    }
    failures
}

fn write_journal(path: &Path, version: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|cause| {
            Error::Install(format!(
                "could not create the install state directory: {cause}"
            ))
        })?;
    }
    let journal = InstallJournal {
        schema: JOURNAL_VERSION,
        package: PACKAGE.to_string(),
        version: version.to_string(),
    };
    let body = serde_json::to_vec_pretty(&journal)
        .map_err(|cause| Error::Install(format!("could not encode install state: {cause}")))?;
    crate::atomic::write(path, body)
        .map_err(|cause| Error::Install(format!("could not commit install state: {cause}")))
}

fn read_journal(path: &Path) -> Result<InstallJournal> {
    let raw = crate::bounded_file::read(path, crate::bounded_file::CONTROL_BYTES)
        .map_err(|cause| Error::Install(format!("could not read install state: {cause}")))?;
    let journal: InstallJournal = serde_json::from_slice(&raw)
        .map_err(|cause| Error::Install(format!("install state is invalid: {cause}")))?;
    if journal.schema != JOURNAL_VERSION || journal.package != PACKAGE {
        return Err(Error::Install(
            "install state belongs to an unsupported runtime transaction".into(),
        ));
    }
    Ok(journal)
}

fn remove_dir_if_exists(path: &Path) -> Result<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(cause) if cause.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(cause) => {
            return Err(Error::Install(format!(
                "could not inspect {}: {cause}",
                path.display()
            )))
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(Error::Install(format!(
            "refusing to recursively remove non-directory or linked path {}",
            path.display()
        )));
    }
    std::fs::remove_dir_all(path)
        .map_err(|cause| Error::Install(format!("could not remove {}: {cause}", path.display())))
}

async fn forward<P, R>(pipe: P, stream: Stream, report: R, activity: mpsc::Sender<()>)
where
    P: tokio::io::AsyncRead + Unpin,
    R: Fn(Stream, String),
{
    let mut lines = BufReader::new(pipe);
    let mut raw = Vec::new();
    while matches!(
        crate::child_output::next_line(&mut lines, &mut raw).await,
        Ok(true)
    ) {
        // Activity is an edge; one pending wakeup is enough to reset the idle
        // deadline and keeps a noisy child from allocating an unbounded queue.
        let _ = activity.try_send(());
        report(stream, String::from_utf8_lossy(&raw).trim_end().to_string());
    }
}

/// Locate npm's entry script next to a Node executable.
///
/// Running `npm-cli.js` with a known Node is exact: it cannot pick up a
/// different runtime from PATH, and on Windows it avoids invoking `npm.cmd`
/// through the command processor.
pub(crate) fn npm_cli(node: &Path) -> Option<PathBuf> {
    npm_cli_candidates(node)
        .into_iter()
        .find(|candidate| candidate.is_file() && npm_cli_works(node, candidate))
}

/// Layouts used by the official archive, version managers and Homebrew.
///
/// Homebrew exposes `node` through `<prefix>/bin`, but canonicalising that
/// symlink (which runtime discovery intentionally does) produces
/// `<prefix>/Cellar/node/<version>/bin/node`. npm is then either formula-owned
/// under `libexec`, or shared under the Homebrew prefix. Keep both candidates:
/// Apple Silicon and Intel Homebrew use the same Cellar shape with different
/// prefixes.
fn npm_cli_candidates(node: &Path) -> Vec<PathBuf> {
    let Some(directory) = node.parent() else {
        return Vec::new();
    };
    vec![
        // Windows: npm sits beside node.exe.
        directory.join("node_modules/npm/bin/npm-cli.js"),
        // Official Unix archives, nvm, fnm and Volta.
        directory.join("../lib/node_modules/npm/bin/npm-cli.js"),
        // Homebrew formula-owned npm from a canonical Cellar node path.
        directory.join("../libexec/lib/node_modules/npm/bin/npm-cli.js"),
        // Homebrew prefix-owned npm from a canonical Cellar node path.
        directory.join("../../../../lib/node_modules/npm/bin/npm-cli.js"),
    ]
}

/// Prove the entry script belongs to a working npm by executing it with the
/// exact Node runtime Studio selected. A same-named shim elsewhere on PATH is
/// never consulted.
fn npm_cli_works(node: &Path, npm_cli: &Path) -> bool {
    let mut command = std::process::Command::new(node);
    command
        .arg(npm_cli)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    proc_guard::hide_console(&mut command);
    let Ok(mut child) = command.spawn() else {
        return false;
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        return false;
    };
    let output = std::thread::spawn(move || crate::child_output::capture_sync(stdout, 16 << 10));
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) | Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let Ok(Ok(output)) = output.join() else {
        return false;
    };
    status.is_some_and(|status| status.success()) && !output.iter().all(u8::is_ascii_whitespace)
}

/// `PATH` with the chosen Node's directory in front.
fn path_with_node(node: &Path) -> OsString {
    let existing = std::env::var_os("PATH").unwrap_or_default();
    let Some(directory) = node.parent() else {
        return existing;
    };

    let mut entries = vec![directory.to_path_buf()];
    entries.extend(std::env::split_paths(&existing));
    std::env::join_paths(entries).unwrap_or(existing)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::time::Duration;

    use tokio::process::Command;

    use super::{
        apply_group, ensure_runtime_resolver, group_applies, npm_cli_candidates, preflight_bodies,
        qualify_runtime, remove_dir_if_exists, replace_once, require_expected_runtime,
        run_command_with_limits, runtime_compatible, runtime_manifest_for, runtime_version,
        InstallPlan, CONNECTION_PATCHES, INTEGRATION_PACKAGE, INTEGRATION_RESOLVER,
        OFFICIAL_REGISTRY, PACKAGE, PNPM_SPEC, PNPM_VERSION, RUNTIME_LOCK, RUNTIME_PACKAGE,
        RUNTIME_SCHEMA, VERSION, spec_for,
    };

    fn write_runtime(root: &Path, version: &str, entry: bool) {
        let package = root.join("node_modules/@deepseek-ai/dsh");
        fs::create_dir_all(package.join("lib")).expect("runtime directory");
        fs::write(
            package.join("package.json"),
            format!(r#"{{"name":"{PACKAGE}","version":"{version}"}}"#),
        )
        .expect("manifest");
        if entry {
            fs::write(package.join("lib/bin.js"), "").expect("entry");
        }
        let pnpm = root.join("node_modules/pnpm");
        fs::create_dir_all(pnpm.join("bin")).expect("pnpm directory");
        fs::write(
            pnpm.join("package.json"),
            format!(r#"{{"name":"pnpm","version":"{PNPM_VERSION}"}}"#),
        )
        .expect("pnpm manifest");
        fs::write(pnpm.join("bin/pnpm.cjs"), "").expect("pnpm entry");
        let integration = root.join("node_modules/@moresyl/dsh-studio-integration/lib");
        fs::create_dir_all(&integration).expect("integration directory");
        fs::write(integration.join("index.js"), "").expect("integration entry");
        fs::write(integration.join("client.js"), "").expect("integration client");
        let picker = root
            .join("node_modules/@deepseek-ai/dsh-client-ui-directory-picker-browse/lib/client.js");
        fs::create_dir_all(picker.parent().expect("picker parent")).expect("picker directory");
        fs::write(
            picker,
            "__DSH_DESKTOP_PICK_DIRECTORY__ __DSH_DESKTOP_VALIDATE_DIRECTORY__",
        )
        .expect("qualified picker");
        let connection = root.join("node_modules/@deepseek-ai/dsh-client-connection/lib/index.js");
        fs::create_dir_all(connection.parent().expect("connection parent"))
            .expect("connection directory");
        fs::write(connection, "process.env.DSH_DESKTOP !== void 0")
            .expect("qualified connection");
        fs::write(
            root.join("dsh-studio-runtime.json"),
            format!(r#"{{"schema":{RUNTIME_SCHEMA}}}"#),
        )
        .expect("runtime marker");
    }

    #[test]
    fn runtime_contract_is_an_exact_package_spec() {
        assert_eq!(spec_for(VERSION), format!("{PACKAGE}@{VERSION}"));
        assert!(!spec_for(VERSION).ends_with("@latest"));
        assert!(!VERSION.starts_with(['^', '~']));
        assert_eq!(PNPM_SPEC, format!("pnpm@{PNPM_VERSION}"));
    }

    #[test]
    fn the_manifest_template_moves_only_lockstep_packages() {
        let manifest = runtime_manifest_for("0.1.5-rc.1").expect("manifest");
        let parsed: serde_json::Value = serde_json::from_slice(&manifest).expect("manifest json");
        let dependencies = parsed
            .get("dependencies")
            .and_then(|table| table.as_object())
            .expect("dependencies");
        let lockstep = dependencies
            .iter()
            .filter(|(name, _)| name.starts_with("@deepseek-ai/"))
            .count();
        let moved = dependencies
            .iter()
            .filter(|(name, pinned)| {
                name.starts_with("@deepseek-ai/") && pinned.as_str() == Some("0.1.5-rc.1")
            })
            .count();
        // Every lockstep package moved except the one at its own version.
        assert_eq!(moved + 1, lockstep);
        assert_eq!(
            dependencies
                .get("@deepseek-ai/cordis-plugin-group")
                .and_then(|pinned| pinned.as_str()),
            Some("1.0.2")
        );
    }

    #[test]
    fn preflight_reads_the_same_seams_the_installer_applies() {
        let connection = "\tisAuthenticated(request) {\n\t\tconst authority = requestAuthority(request.headers);\n}";
        let report = preflight_bodies("x", connection, "no picker seams here");
        assert!(report.connection);
        assert!(!report.picker);
        let applied = apply_group(connection.to_string(), CONNECTION_PATCHES).expect("applied");
        assert!(applied.contains("process.env.DSH_DESKTOP !== void 0"));
        assert!(group_applies(&applied, CONNECTION_PATCHES));
    }

    #[test]
    fn npm_candidates_cover_official_and_version_manager_layouts() {
        let node = Path::new("/Users/person/.nvm/versions/node/v24.19.0/bin/node");
        let candidates = npm_cli_candidates(node);
        assert!(candidates.contains(&Path::new(
            "/Users/person/.nvm/versions/node/v24.19.0/bin/../lib/node_modules/npm/bin/npm-cli.js"
        ).to_path_buf()));
    }

    #[test]
    fn npm_candidates_cover_both_homebrew_prefixes_after_canonicalization() {
        for prefix in ["/opt/homebrew", "/usr/local"] {
            let node = Path::new(prefix).join("Cellar/node/26.7.0/bin/node");
            let candidates = npm_cli_candidates(&node);
            assert!(candidates.contains(
                &node
                    .parent()
                    .expect("bin")
                    .join("../libexec/lib/node_modules/npm/bin/npm-cli.js")
            ));
            assert!(candidates.contains(
                &node
                    .parent()
                    .expect("bin")
                    .join("../../../../lib/node_modules/npm/bin/npm-cli.js")
            ));
        }
    }

    #[test]
    fn managed_install_uses_the_official_registry_and_exposes_lifecycle_progress() {
        let plan = InstallPlan {
            node: Path::new("node").to_path_buf(),
            npm_cli: Path::new("npm-cli.js").to_path_buf(),
            target: Path::new("runtime").to_path_buf(),
            spec: spec_for(VERSION),
            version: VERSION.to_string(),
        };
        let locked = plan.to_locked_command();
        let arguments = locked
            .as_std()
            .get_args()
            .map(|value| value.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(arguments.contains(&"--foreground-scripts".to_string()));
        assert!(arguments.contains(&"--install-links".to_string()));
        assert!(arguments.contains(&format!("--registry={OFFICIAL_REGISTRY}")));
        assert!(arguments.contains(&"--fetch-timeout=60000".to_string()));

        let plugin = plan.to_command();
        let plugin_arguments = plugin
            .as_std()
            .get_args()
            .map(|value| value.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(plugin_arguments.contains(&"--foreground-scripts".to_string()));
        assert!(!plugin_arguments
            .iter()
            .any(|value| value.starts_with("--registry=")));
    }

    #[test]
    fn silent_install_fixture() {
        if std::env::var_os("DSH_STUDIO_SILENT_INSTALL_FIXTURE").is_some() {
            std::thread::sleep(Duration::from_secs(10));
        }
    }

    #[tokio::test]
    async fn a_silent_install_is_stopped_instead_of_waiting_forever() {
        let mut command = Command::new(std::env::current_exe().expect("test executable"));
        command
            .arg("--exact")
            .arg("harness::install::tests::silent_install_fixture")
            .arg("--nocapture")
            .env("DSH_STUDIO_SILENT_INSTALL_FIXTURE", "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let failure = run_command_with_limits(
            command,
            |_, _| {},
            "npm ci",
            Duration::from_millis(500),
            Duration::from_secs(5),
            Duration::from_millis(100),
        )
        .await
        .expect_err("silent fixture should time out");
        assert!(failure.to_string().contains("produced no output"));
    }

    #[test]
    fn embedded_runtime_lock_matches_the_qualified_versions() {
        let package: serde_json::Value =
            serde_json::from_slice(RUNTIME_PACKAGE).expect("runtime package contract");
        let dependencies = package["dependencies"]
            .as_object()
            .expect("runtime dependencies");
        assert_eq!(dependencies[PACKAGE], VERSION);
        assert_eq!(dependencies["pnpm"], PNPM_VERSION);
        assert_eq!(
            dependencies[INTEGRATION_PACKAGE],
            "file:dsh-studio-integration"
        );
        assert!(dependencies
            .values()
            .all(|version| version.as_str().is_some_and(|version| {
                !version.starts_with(['^', '~']) && !version.contains('*')
            })));

        let lock: serde_json::Value =
            serde_json::from_slice(RUNTIME_LOCK).expect("runtime package lock");
        assert_eq!(lock["lockfileVersion"], 3);
        assert_eq!(
            lock["packages"][""]["dependencies"],
            package["dependencies"]
        );
        let serialized = String::from_utf8_lossy(RUNTIME_LOCK);
        assert!(!serialized.contains("registry.npmmirror.com"));
        assert!(serialized.contains("https://registry.npmjs.org/"));
    }

    #[test]
    fn compatibility_requires_the_exact_version_and_entry() {
        let root = std::env::temp_dir().join(format!(
            "dsh-studio-runtime-contract-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&root);
        write_runtime(&root, VERSION, true);
        assert_eq!(runtime_version(&root).as_deref(), Some(VERSION));
        assert!(runtime_compatible(&root));

        fs::remove_file(root.join("dsh-studio-runtime.json")).expect("remove marker");
        assert!(!runtime_compatible(&root));
        write_runtime(&root, VERSION, true);

        write_runtime(&root, "0.0.1-rc.1", true);
        assert!(!runtime_compatible(&root));
        write_runtime(&root, VERSION, false);
        let _ = fs::remove_file(root.join("node_modules/@deepseek-ai/dsh/lib/bin.js"));
        assert!(!runtime_compatible(&root));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn launch_support_materializes_and_repairs_the_runtime_resolver() {
        let root = std::env::temp_dir().join(format!(
            "dsh-studio-runtime-resolver-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        write_runtime(&root, VERSION, true);

        let resolver = ensure_runtime_resolver(&root).expect("materialize resolver");
        assert_eq!(
            fs::read(&resolver).expect("resolver body"),
            INTEGRATION_RESOLVER
        );
        fs::write(&resolver, "stale resolver").expect("stale resolver fixture");
        assert_eq!(
            ensure_runtime_resolver(&root).expect("repair resolver"),
            resolver
        );
        assert_eq!(
            fs::read(&resolver).expect("repaired body"),
            INTEGRATION_RESOLVER
        );

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn launch_support_refuses_an_unsafe_runtime_resolver() {
        let root = std::env::temp_dir().join(format!(
            "dsh-studio-runtime-resolver-kind-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        write_runtime(&root, VERSION, true);
        let resolver =
            root.join("node_modules/@moresyl/dsh-studio-integration/lib/runtime-resolver.cjs");
        fs::create_dir(&resolver).expect("unsafe resolver directory");

        let failure = ensure_runtime_resolver(&root).expect_err("directory must be refused");
        assert!(failure.to_string().contains("unsafe file"));
        assert!(resolver.is_dir());

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn contract_failure_names_the_missing_studio_integration() {
        let root = std::env::temp_dir().join(format!(
            "dsh-studio-runtime-contract-failure-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        write_runtime(&root, VERSION, true);
        fs::remove_file(root.join("node_modules/@moresyl/dsh-studio-integration/lib/client.js"))
            .expect("remove integration entry");

        let failure = require_expected_runtime(&root).expect_err("contract should fail");
        assert!(failure.to_string().contains("failed: Studio integration"));

        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn recursive_cleanup_refuses_an_unexpected_file() {
        let path =
            std::env::temp_dir().join(format!("dsh-studio-runtime-cleanup-{}", std::process::id()));
        let _ = fs::remove_file(&path);
        fs::write(&path, "not a directory").expect("file");
        assert!(remove_dir_if_exists(&path).is_err());
        assert!(path.is_file(), "the refused target must remain untouched");
        fs::remove_file(path).expect("cleanup");
    }

    #[test]
    fn qualification_replacement_is_idempotent_even_when_it_keeps_the_seam() {
        let once = replace_once(
            "before seam after".into(),
            "seam",
            "addition seam",
            "fixture",
        )
        .expect("first qualification");
        let twice = replace_once(once.clone(), "seam", "addition seam", "fixture")
            .expect("second qualification");
        assert_eq!(twice, once);
    }

    #[test]
    fn the_locally_installed_locked_picker_accepts_the_qualification() {
        let source = crate::paths::harness_dir()
            .join("node_modules/@deepseek-ai/dsh-client-ui-directory-picker-browse/lib/client.js");
        if !source.is_file() {
            return;
        }
        let root = std::env::temp_dir().join(format!(
            "dsh-studio-picker-qualification-{}",
            std::process::id()
        ));
        let target = root
            .join("node_modules/@deepseek-ai/dsh-client-ui-directory-picker-browse/lib/client.js");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(target.parent().expect("picker parent")).expect("picker directory");
        fs::copy(source, &target).expect("copy locked picker");
        let connection = root.join("node_modules/@deepseek-ai/dsh-client-connection/lib/index.js");
        fs::create_dir_all(connection.parent().expect("connection parent"))
            .expect("connection directory");
        fs::write(
            &connection,
            "\tisAuthenticated(request) {\n\t\tconst authority = requestAuthority(request.headers);\n",
        )
        .expect("connection fixture");

        qualify_runtime(&root).expect("qualify locked picker");
        let patched = fs::read_to_string(target).expect("patched picker");
        assert!(patched.contains("__DSH_DESKTOP_PICK_DIRECTORY__"));
        assert!(patched.contains("openDirectory(targetPath)"));
        let connection = fs::read_to_string(connection).expect("patched connection");
        assert!(connection.contains("process.env.DSH_DESKTOP !== void 0"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn qualification_refuses_an_oversized_picker_bundle() {
        let root = std::env::temp_dir().join(format!(
            "dsh-studio-picker-oversized-{}",
            std::process::id()
        ));
        let target = root
            .join("node_modules/@deepseek-ai/dsh-client-ui-directory-picker-browse/lib/client.js");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(target.parent().expect("picker parent")).expect("picker directory");
        fs::write(&target, vec![b' '; crate::bounded_file::CONTROL_BYTES + 1])
            .expect("oversized picker");

        let failure = qualify_runtime(&root).expect_err("oversized picker must be refused");

        assert!(failure.to_string().contains("safety limit"));
        assert_eq!(
            fs::metadata(&target).expect("picker remains").len(),
            (crate::bounded_file::CONTROL_BYTES + 1) as u64
        );
        let _ = fs::remove_dir_all(root);
    }
}
