use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use once_cell::sync::Lazy;
use once_cell::sync::OnceCell;
use parking_lot::Mutex;
use std::ffi::OsString;
use std::fs;
use std::hash::Hash;
use std::num::NonZeroUsize;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::SystemTime;
use sysinfo::System;

use dprint_core::async_runtime::async_trait;

use super::CanonicalizedPathBuf;
use super::DirEntry;
use super::Environment;
use super::FilePermissions;
use super::UrlDownloader;
use crate::plugins::CompilationResult;
use crate::utils::log_action_with_progress;
use crate::utils::show_confirm;
use crate::utils::show_multi_select;
use crate::utils::show_select;
use crate::utils::FastInsecureHasher;
use crate::utils::LogLevel;
use crate::utils::Logger;
use crate::utils::LoggerOptions;
use crate::utils::NoProxy;
use crate::utils::ProgressBars;
use crate::utils::RealUrlDownloader;
use crate::utils::UnsafelyIgnoreCertificates;

// cache the cwd because it's much faster than looking it up each time
static CACHED_CWD: OnceCell<CanonicalizedPathBuf> = OnceCell::new();

pub struct RealEnvironmentOptions {
  pub log_level: LogLevel,
  pub is_stdout_machine_readable: bool,
}

#[derive(Clone)]
pub struct RealEnvironment {
  progress_bars: Option<Arc<ProgressBars>>,
  url_downloader: Arc<RealUrlDownloader>,
  logger: Arc<Logger>,
  system: Arc<Mutex<System>>,
}

impl RealEnvironment {
  pub fn new(options: RealEnvironmentOptions) -> Result<RealEnvironment> {
    let logger = Arc::new(Logger::new(&LoggerOptions {
      initial_context_name: "dprint".to_string(),
      is_stdout_machine_readable: options.is_stdout_machine_readable,
      log_level: options.log_level,
    }));
    let progress_bars = ProgressBars::new(&logger).map(Arc::new);
    let no_proxy = NoProxy::from_env();
    let url_downloader = Arc::new(RealUrlDownloader::new(
      progress_bars.clone(),
      logger.clone(),
      no_proxy,
      UnsafelyIgnoreCertificates::from_env(),
    )?);
    let environment = RealEnvironment {
      url_downloader,
      logger,
      progress_bars,
      system: Default::default(),
    };

    // ensure the cache directory is created
    match (*CACHE_DIR).as_ref() {
      Ok(cache_dir) => cache_dir,
      Err(err) => {
        bail!("Error creating cache directory: {:#}", err);
      }
    };

    Ok(environment)
  }

  #[cfg(test)]
  pub fn run_test_with_real_env(run_with_env: impl Fn(RealEnvironment) -> dprint_core::async_runtime::LocalBoxFuture<'static, ()>) {
    let rt = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
    let env = RealEnvironment::new(RealEnvironmentOptions {
      log_level: LogLevel::Info,
      is_stdout_machine_readable: false,
    })
    .unwrap();

    rt.block_on(async move { run_with_env(env).await });
  }
}

#[async_trait(?Send)]
impl UrlDownloader for RealEnvironment {
  async fn download_file(&self, url: &str) -> Result<Option<Vec<u8>>> {
    log_debug!(self, "Downloading url: {}", url);

    let downloader = self.url_downloader.clone();
    let url = url.to_string();
    dprint_core::async_runtime::spawn_blocking(move || downloader.download(&url)).await?
  }
}

#[async_trait]
impl Environment for RealEnvironment {
  fn is_real(&self) -> bool {
    true
  }

  fn env_var(&self, name: &str) -> Option<OsString> {
    std::env::var_os(name)
  }

  fn read_file(&self, file_path: impl AsRef<Path>) -> Result<String> {
    Ok(String::from_utf8(self.read_file_bytes(file_path)?)?)
  }

  fn read_file_bytes(&self, file_path: impl AsRef<Path>) -> Result<Vec<u8>> {
    log_debug!(self, "Reading file: {}", file_path.as_ref().display());
    #[allow(clippy::disallowed_methods)]
    match fs::read(&file_path) {
      Ok(bytes) => Ok(bytes),
      Err(err) => bail!("Error reading file {}: {:#}", file_path.as_ref().display(), err),
    }
  }

  fn get_staged_files(&self) -> Result<Vec<PathBuf>> {
    let output = Command::new("git")
      .arg("diff")
      .arg("--name-only")
      .arg("--relative")
      .arg("--staged")
      .arg("--diff-filter=ACMR")
      .output()?;

    Ok(String::from_utf8_lossy(&output.stdout).lines().map(PathBuf::from).collect())
  }

  fn write_file_bytes(&self, file_path: impl AsRef<Path>, bytes: &[u8]) -> Result<()> {
    log_debug!(self, "Writing file: {}", file_path.as_ref().display());
    #[allow(clippy::disallowed_methods)]
    match fs::write(&file_path, bytes) {
      Ok(_) => Ok(()),
      Err(err) => bail!("Error writing file {}: {:#}", file_path.as_ref().display(), err),
    }
  }

  fn rename(&self, path_from: impl AsRef<Path>, path_to: impl AsRef<Path>) -> Result<()> {
    log_debug!(self, "Renaming {} -> {}", path_from.as_ref().display(), path_to.as_ref().display());
    #[allow(clippy::disallowed_methods)]
    fs::rename(&path_from, &path_to).with_context(|| format!("Error renaming {} to {}", path_from.as_ref().display(), path_to.as_ref().display()))
  }

  fn remove_file(&self, file_path: impl AsRef<Path>) -> Result<()> {
    log_debug!(self, "Deleting file: {}", file_path.as_ref().display());
    #[allow(clippy::disallowed_methods)]
    match fs::remove_file(&file_path) {
      Ok(_) => Ok(()),
      Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
      Err(err) => bail!("Error deleting file {}: {:#}", file_path.as_ref().display(), err),
    }
  }

  fn remove_dir_all(&self, dir_path: impl AsRef<Path>) -> Result<()> {
    log_debug!(self, "Deleting directory: {}", dir_path.as_ref().display());
    #[allow(clippy::disallowed_methods)]
    match fs::remove_dir_all(&dir_path) {
      Ok(_) => Ok(()),
      Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
      Err(err) => bail!("Error removing directory {}: {:#}", dir_path.as_ref().display(), err),
    }
  }

  fn dir_info(&self, dir_path: impl AsRef<Path>) -> std::io::Result<Vec<DirEntry>> {
    let mut entries = Vec::new();

    #[allow(clippy::disallowed_methods)]
    let dir_info = std::fs::read_dir(&dir_path)?;

    for entry in dir_info {
      let entry = entry?;
      let file_type = entry.file_type()?;
      if file_type.is_dir() {
        entries.push(DirEntry::Directory(entry.path()));
      } else if file_type.is_file() {
        entries.push(DirEntry::File {
          name: entry.file_name(),
          path: entry.path(),
        });
      }
    }

    Ok(entries)
  }

  fn path_exists(&self, file_path: impl AsRef<Path>) -> bool {
    log_debug!(self, "Checking path exists: {}", file_path.as_ref().display());
    #[allow(clippy::disallowed_methods)]
    file_path.as_ref().exists()
  }

  fn canonicalize(&self, path: impl AsRef<Path>) -> Result<CanonicalizedPathBuf> {
    canonicalize_path(path)
  }

  fn is_absolute_path(&self, path: impl AsRef<Path>) -> bool {
    path.as_ref().is_absolute()
  }

  fn file_permissions(&self, path: impl AsRef<Path>) -> Result<FilePermissions> {
    Ok(FilePermissions::Std(
      #[allow(clippy::disallowed_methods)]
      fs::metadata(&path)
        .with_context(|| format!("Error getting file permissions for: {}", path.as_ref().display()))?
        .permissions(),
    ))
  }

  fn set_file_permissions(&self, path: impl AsRef<Path>, permissions: FilePermissions) -> Result<()> {
    let permissions = match permissions {
      FilePermissions::Std(p) => p,
      _ => panic!("Programming error. Permissions did not contain an std permission."),
    };
    #[allow(clippy::disallowed_methods)]
    fs::set_permissions(&path, permissions).with_context(|| format!("Error setting file permissions for: {}", path.as_ref().display()))?;
    Ok(())
  }

  fn mk_dir_all(&self, path: impl AsRef<Path>) -> Result<()> {
    log_debug!(self, "Creating directory: {}", path.as_ref().display());
    #[allow(clippy::disallowed_methods)]
    match fs::create_dir_all(&path) {
      Ok(_) => Ok(()),
      Err(err) => bail!("Error creating directory {}: {:#}", path.as_ref().display(), err),
    }
  }

  fn cwd(&self) -> CanonicalizedPathBuf {
    CACHED_CWD
      .get_or_init(|| {
        #[allow(clippy::disallowed_methods)]
        self
          .canonicalize(std::env::current_dir().expect("Expected to get the current working directory."))
          .expect("expected to canonicalize the cwd")
      })
      .clone()
  }

  fn current_exe(&self) -> Result<PathBuf> {
    std::env::current_exe().context("Error getting current executable.")
  }

  fn __log__(&self, text: &str) {
    self.logger.log(text, "dprint");
  }

  fn log_machine_readable(&self, bytes: &[u8]) {
    self.logger.log_machine_readable(bytes);
  }

  fn log_stderr_with_context(&self, text: &str, context_name: &str) {
    self.logger.log_stderr_with_context(text, context_name);
  }

  fn log_action_with_progress<TResult: Send + Sync, TCreate: FnOnce(Box<dyn Fn(usize)>) -> TResult + Send + Sync>(
    &self,
    message: &str,
    action: TCreate,
    total_size: usize,
  ) -> TResult {
    log_action_with_progress(self.progress_bars.as_deref(), message, action, total_size)
  }

  fn get_cache_dir(&self) -> CanonicalizedPathBuf {
    // ok to unwrap because this would have errored in the constructor
    (*CACHE_DIR.as_ref().unwrap()).clone()
  }

  fn get_home_dir(&self) -> Option<CanonicalizedPathBuf> {
    dirs::home_dir().map(|path| self.canonicalize(path).unwrap())
  }

  fn cpu_arch(&self) -> String {
    std::env::consts::ARCH.to_string()
  }

  fn os(&self) -> String {
    let target = env!("TARGET");
    if target.contains("linux-musl") {
      "linux-musl".to_string()
    } else {
      std::env::consts::OS.to_string()
    }
  }

  fn max_threads(&self) -> usize {
    #[allow(clippy::disallowed_methods)]
    resolve_max_threads(std::env::var("DPRINT_MAX_THREADS").ok(), std::thread::available_parallelism().ok())
  }

  fn cli_version(&self) -> String {
    env!("CARGO_PKG_VERSION").to_string()
  }

  fn get_time_secs(&self) -> u64 {
    SystemTime::now().duration_since(std::time::SystemTime::UNIX_EPOCH).unwrap().as_secs()
  }

  fn get_selection(&self, prompt_message: &str, item_indent_width: u16, items: &[String]) -> Result<usize> {
    show_select(&self.logger, "dprint", prompt_message, item_indent_width, items)
  }

  fn get_multi_selection(&self, prompt_message: &str, item_indent_width: u16, items: &[(bool, String)]) -> Result<Vec<usize>> {
    show_multi_select(
      &self.logger,
      "dprint",
      prompt_message,
      item_indent_width,
      items.iter().map(|(value, text)| (value.to_owned(), text)).collect(),
    )
  }

  fn confirm(&self, prompt_message: &str, default_value: bool) -> Result<bool> {
    show_confirm(&self.logger, "dprint", prompt_message, default_value)
  }

  fn is_ci(&self) -> bool {
    match std::env::var_os("CI") {
      Some(value) => {
        let value = value.to_string_lossy();
        matches!(value.as_ref(), "true" | "1")
      }
      None => false,
    }
  }

  #[inline]
  fn log_level(&self) -> LogLevel {
    self.logger.log_level()
  }

  fn compile_wasm(&self, wasm_bytes: &[u8]) -> Result<CompilationResult> {
    crate::plugins::compile_wasm(wasm_bytes)
  }

  fn wasm_cache_key(&self) -> String {
    let cpu = self.cpu_arch();
    // need to also hash on the CPU features
    // https://github.com/dprint/dprint/issues/735
    let mut hash = FastInsecureHasher::default();
    let mut features = wasmer::sys::CpuFeature::for_host().into_iter().map(|c| c.to_string()).collect::<Vec<_>>();
    features.sort(); // ensure this is stable
    for feature in features {
      feature.hash(&mut hash);
    }
    // include the rustc version in the hash because wasmer's deserialization
    // of wasm plugins sometimes breaks with a rust upgrade
    env!("RUSTC_VERSION_TEXT").hash(&mut hash);
    format!("{}-{}", cpu, hash.finish())
  }

  async fn cpu_usage(&self) -> u8 {
    // the documentation recommends calling this twice in order
    // to get a more accurate cpu reading
    let system = self.system.clone();
    let Ok(system) = dprint_core::async_runtime::spawn_blocking(move || {
      {
        let mut system = system.lock();
        system.refresh_cpu_usage();
      }
      system
    })
    .await
    else {
      return 0;
    };

    // wait a duration that allows getting a more accurate cpu usage
    tokio::time::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL).await;

    dprint_core::async_runtime::spawn_blocking(move || {
      let mut system = system.lock();
      system.refresh_cpu_usage();
      let utilization = system.cpus().iter().map(|c| c.cpu_usage()).sum::<f32>() / system.cpus().len() as f32;
      if utilization > 101f32 {
        0 // something wrong, so just return 0 for "cannot figure out cpu usage"
      } else {
        utilization as u8
      }
    })
    .await
    .unwrap_or(0)
  }

  fn stdout(&self) -> Box<dyn std::io::Write + Send> {
    Box::new(std::io::stdout())
  }

  fn stdin(&self) -> Box<dyn std::io::Read + Send> {
    Box::new(std::io::stdin())
  }

  fn progress_bars(&self) -> Option<&Arc<ProgressBars>> {
    self.progress_bars.as_ref()
  }

  #[cfg(windows)]
  fn ensure_system_path(&self, directory_path: &str) -> Result<()> {
    use winreg::enums::*;
    use winreg::RegKey;
    log_debug!(self, "Ensuring '{}' is on the path.", directory_path);

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (env, _) = hkcu.create_subkey("Environment")?;
    let mut path: String = env.get_value("Path")?;

    // add to the path if it doesn't have this entry
    if !path.split(';').any(|p| p == directory_path) {
      if !path.is_empty() && !path.ends_with(';') {
        path.push(';')
      }
      path.push_str(directory_path);
      env.set_value("Path", &path)?;
    }
    Ok(())
  }

  #[cfg(windows)]
  fn remove_system_path(&self, directory_path: &str) -> Result<()> {
    use winreg::enums::*;
    use winreg::RegKey;
    log_debug!(self, "Ensuring '{}' is on the path.", directory_path);

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (env, _) = hkcu.create_subkey("Environment")?;
    let path: String = env.get_value("Path")?;
    let mut paths = path.split(';').collect::<Vec<_>>();
    let original_len = paths.len();

    paths.retain(|p| p != &directory_path);

    let was_removed = original_len != paths.len();
    if was_removed {
      env.set_value("Path", &paths.join(";"))?;
    }
    Ok(())
  }
}

fn resolve_max_threads(env_var: Option<String>, available_parallelism: Option<NonZeroUsize>) -> usize {
  fn maybe_specified_threads(env_var: Option<String>) -> Option<usize> {
    let value = env_var?.parse::<usize>().ok()?;
    if value > 0 {
      Some(value)
    } else {
      None
    }
  }

  let maybe_actual_count = available_parallelism.map(|p| p.get());
  match maybe_specified_threads(env_var) {
    Some(specified_count) => match maybe_actual_count {
      Some(actual_count) if specified_count > actual_count => actual_count,
      _ => specified_count,
    },
    _ => maybe_actual_count.unwrap_or(4),
  }
}

fn canonicalize_path(path: impl AsRef<Path>) -> Result<CanonicalizedPathBuf> {
  // use this to avoid //?//C:/etc... like paths on windows (UNC)
  match dunce::canonicalize(path.as_ref()) {
    Ok(result) => Ok(CanonicalizedPathBuf::new(result)),
    Err(err) => bail!("Error canonicalizing path {}: {:#}", path.as_ref().display(), err),
  }
}

const CACHE_DIR_ENV_VAR_NAME: &str = "DPRINT_CACHE_DIR";

static CACHE_DIR: Lazy<Result<CanonicalizedPathBuf>> = Lazy::new(|| {
  #[allow(clippy::disallowed_methods)]
  let cache_dir = get_cache_dir_internal(|var_name| std::env::var(var_name).ok())?;
  #[allow(clippy::disallowed_methods)]
  std::fs::create_dir_all(&cache_dir)?;
  canonicalize_path(cache_dir)
});

fn get_cache_dir_internal(get_env_var: impl Fn(&str) -> Option<String>) -> Result<PathBuf> {
  if let Some(dir_path) = get_env_var(CACHE_DIR_ENV_VAR_NAME) {
    if !dir_path.trim().is_empty() {
      let dir_path = PathBuf::from(dir_path);
      // seems dangerous to allow a relative path as this directory may be deleted
      return if !dir_path.is_absolute() {
        bail!("The {} environment variable must specify an absolute path.", CACHE_DIR_ENV_VAR_NAME)
      } else {
        Ok(dir_path)
      };
    }
  }

  match dirs::cache_dir() {
    Some(dir) => Ok(dir.join("dprint").join("cache")),
    None => bail!("Expected to find cache directory"),
  }
}

#[cfg(test)]
mod test {
  use super::*;

  #[test]
  fn should_get_cache_dir_based_on_env_var() {
    let default_dir = dirs::cache_dir().unwrap().join("dprint").join("cache");
    let value = if cfg!(target_os = "windows") {
      "C:/.dprint-cache"
    } else {
      "/home/david/.dprint-cache"
    };
    assert_eq!(get_cache_dir_internal(|_| Some(value.to_string())).unwrap().to_string_lossy(), value);
    assert_eq!(get_cache_dir_internal(|_| Some("".to_string())).unwrap(), default_dir);
    assert_eq!(get_cache_dir_internal(|_| Some("  ".to_string())).unwrap(), default_dir);
    assert_eq!(get_cache_dir_internal(|_| None).unwrap(), default_dir);
  }

  #[test]
  fn should_error_when_cache_dir_env_var_relative() {
    let result = get_cache_dir_internal(|_| Some("./dir".to_string())).err();
    assert_eq!(
      result.unwrap().to_string(),
      "The DPRINT_CACHE_DIR environment variable must specify an absolute path."
    );
  }

  #[test]
  fn should_resolve_num_threads() {
    assert_eq!(resolve_max_threads(None, None), 4);
    assert_eq!(resolve_max_threads(None, NonZeroUsize::new(1)), 1);
    assert_eq!(resolve_max_threads(None, NonZeroUsize::new(4)), 4);
    assert_eq!(resolve_max_threads(Some("2".to_string()), NonZeroUsize::new(4)), 2);
    assert_eq!(resolve_max_threads(Some("0".to_string()), NonZeroUsize::new(4)), 4);
    assert_eq!(resolve_max_threads(Some("5".to_string()), NonZeroUsize::new(4)), 4);
    assert_eq!(resolve_max_threads(Some("4".to_string()), NonZeroUsize::new(4)), 4);
  }
}
