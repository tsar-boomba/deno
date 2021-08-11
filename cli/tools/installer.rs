// Copyright 2018-2021 the Deno authors. All rights reserved. MIT license.
use crate::flags::Flags;
use crate::fs_util::canonicalize_path;
use deno_core::error::generic_error;
use deno_core::error::AnyError;
use deno_core::resolve_url_or_path;
use deno_core::url::Url;
use log::Level;
use regex::Regex;
use regex::RegexBuilder;
use std::env;
use std::fs;
use std::fs::File;
use std::io;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;

#[cfg(not(windows))]
use std::os::unix::fs::PermissionsExt;

lazy_static::lazy_static! {
    static ref EXEC_NAME_RE: Regex = RegexBuilder::new(
        r"^[a-z][\w-]*$"
    ).case_insensitive(true).build().unwrap();
    // Regular expression to test disk driver letter. eg "C:\\User\username\path\to"
    static ref DRIVE_LETTER_REG: Regex = RegexBuilder::new(
        r"^[c-z]:"
    ).case_insensitive(true).build().unwrap();
}

fn validate_name(exec_name: &str) -> Result<(), AnyError> {
  if EXEC_NAME_RE.is_match(exec_name) {
    Ok(())
  } else {
    Err(generic_error(format!(
      "Invalid executable name: {}",
      exec_name
    )))
  }
}

#[cfg(windows)]
/// On Windows, 2 files are generated.
/// One compatible with cmd & powershell with a .cmd extension
/// A second compatible with git bash / MINGW64
/// Generate batch script to satisfy that.
fn generate_executable_file(
  mut file_path: PathBuf,
  args: Vec<String>,
) -> Result<(), AnyError> {
  let args: Vec<String> = args.iter().map(|c| format!("\"{}\"", c)).collect();
  let template = format!(
    "% generated by deno install %\n@deno {} %*\n",
    args
      .iter()
      .map(|arg| arg.replace("%", "%%"))
      .collect::<Vec<_>>()
      .join(" ")
  );
  let mut file = File::create(&file_path)?;
  file.write_all(template.as_bytes())?;

  // write file for bash
  // create filepath without extensions
  file_path.set_extension("");
  let template = format!(
    r#"#!/bin/sh
# generated by deno install
deno {} "$@"
"#,
    args.join(" "),
  );
  let mut file = File::create(&file_path)?;
  file.write_all(template.as_bytes())?;
  Ok(())
}

#[cfg(not(windows))]
fn generate_executable_file(
  file_path: PathBuf,
  args: Vec<String>,
) -> Result<(), AnyError> {
  use shell_escape::escape;
  let args: Vec<String> = args
    .into_iter()
    .map(|c| escape(c.into()).into_owned())
    .collect();
  let template = format!(
    r#"#!/bin/sh
# generated by deno install
exec deno {} "$@"
"#,
    args.join(" "),
  );
  let mut file = File::create(&file_path)?;
  file.write_all(template.as_bytes())?;
  let _metadata = fs::metadata(&file_path)?;
  let mut permissions = _metadata.permissions();
  permissions.set_mode(0o755);
  fs::set_permissions(&file_path, permissions)?;
  Ok(())
}

fn get_installer_root() -> Result<PathBuf, io::Error> {
  if let Ok(env_dir) = env::var("DENO_INSTALL_ROOT") {
    if !env_dir.is_empty() {
      return canonicalize_path(&PathBuf::from(env_dir));
    }
  }
  // Note: on Windows, the $HOME environment variable may be set by users or by
  // third party software, but it is non-standard and should not be relied upon.
  let home_env_var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
  let mut home_path =
    env::var_os(home_env_var)
      .map(PathBuf::from)
      .ok_or_else(|| {
        io::Error::new(
          io::ErrorKind::NotFound,
          format!("${} is not defined", home_env_var),
        )
      })?;
  home_path.push(".deno");
  Ok(home_path)
}

pub fn infer_name_from_url(url: &Url) -> Option<String> {
  let path = PathBuf::from(url.path());
  let mut stem = match path.file_stem() {
    Some(stem) => stem.to_string_lossy().to_string(),
    None => return None,
  };
  if stem == "main" || stem == "mod" || stem == "index" || stem == "cli" {
    if let Some(parent_name) = path.parent().and_then(|p| p.file_name()) {
      stem = parent_name.to_string_lossy().to_string();
    }
  }
  let stem = stem.splitn(2, '@').next().unwrap().to_string();
  Some(stem)
}

pub fn install(
  flags: Flags,
  module_url: &str,
  args: Vec<String>,
  name: Option<String>,
  root: Option<PathBuf>,
  force: bool,
) -> Result<(), AnyError> {
  let root = if let Some(root) = root {
    canonicalize_path(&root)?
  } else {
    get_installer_root()?
  };
  let installation_dir = root.join("bin");

  // ensure directory exists
  if let Ok(metadata) = fs::metadata(&installation_dir) {
    if !metadata.is_dir() {
      return Err(generic_error("Installation path is not a directory"));
    }
  } else {
    fs::create_dir_all(&installation_dir)?;
  };

  // Check if module_url is remote
  let module_url = resolve_url_or_path(module_url)?;

  let name = name.or_else(|| infer_name_from_url(&module_url));

  let name = match name {
    Some(name) => name,
    None => return Err(generic_error(
      "An executable name was not provided. One could not be inferred from the URL. Aborting.",
    )),
  };

  validate_name(name.as_str())?;
  let mut file_path = installation_dir.join(&name);

  if cfg!(windows) {
    file_path = file_path.with_extension("cmd");
  }

  if file_path.exists() && !force {
    return Err(generic_error(
      "Existing installation found. Aborting (Use -f to overwrite).",
    ));
  };

  let mut extra_files: Vec<(PathBuf, String)> = vec![];

  let mut executable_args = vec!["run".to_string()];
  executable_args.extend_from_slice(&flags.to_permission_args());
  if let Some(url) = flags.location.as_ref() {
    executable_args.push("--location".to_string());
    executable_args.push(url.to_string());
  }
  if let Some(ca_file) = flags.ca_file {
    executable_args.push("--cert".to_string());
    executable_args.push(ca_file)
  }
  if let Some(log_level) = flags.log_level {
    if log_level == Level::Error {
      executable_args.push("--quiet".to_string());
    } else {
      executable_args.push("--log-level".to_string());
      let log_level = match log_level {
        Level::Debug => "debug",
        Level::Info => "info",
        _ => {
          return Err(generic_error(format!("invalid log level {}", log_level)))
        }
      };
      executable_args.push(log_level.to_string());
    }
  }

  if flags.no_check {
    executable_args.push("--no-check".to_string());
  }

  if flags.unstable {
    executable_args.push("--unstable".to_string());
  }

  if flags.no_remote {
    executable_args.push("--no-remote".to_string());
  }

  if flags.lock_write {
    executable_args.push("--lock-write".to_string());
  }

  if flags.cached_only {
    executable_args.push("--cached-only".to_string());
  }

  if !flags.v8_flags.is_empty() {
    executable_args.push(format!("--v8-flags={}", flags.v8_flags.join(",")));
  }

  if let Some(seed) = flags.seed {
    executable_args.push("--seed".to_string());
    executable_args.push(seed.to_string());
  }

  if let Some(inspect) = flags.inspect {
    executable_args.push(format!("--inspect={}", inspect.to_string()));
  }

  if let Some(inspect_brk) = flags.inspect_brk {
    executable_args.push(format!("--inspect-brk={}", inspect_brk.to_string()));
  }

  if let Some(import_map_path) = flags.import_map_path {
    let import_map_url = resolve_url_or_path(&import_map_path)?;
    executable_args.push("--import-map".to_string());
    executable_args.push(import_map_url.to_string());
  }

  if let Some(config_path) = flags.config_path {
    let mut copy_path = file_path.clone();
    copy_path.set_extension("tsconfig.json");
    executable_args.push("--config".to_string());
    executable_args.push(copy_path.to_str().unwrap().to_string());
    extra_files.push((copy_path, fs::read_to_string(config_path)?));
  }

  if let Some(lock_path) = flags.lock {
    let mut copy_path = file_path.clone();
    copy_path.set_extension("lock.json");
    executable_args.push("--lock".to_string());
    executable_args.push(copy_path.to_str().unwrap().to_string());
    extra_files.push((copy_path, fs::read_to_string(lock_path)?));
  }

  executable_args.push(module_url.to_string());
  executable_args.extend_from_slice(&args);

  generate_executable_file(file_path.to_owned(), executable_args)?;
  for (path, contents) in extra_files {
    fs::write(path, contents)?;
  }

  println!("✅ Successfully installed {}", name);
  println!("{}", file_path.to_string_lossy());
  if cfg!(windows) {
    file_path.set_extension("");
    println!("{} (shell)", file_path.to_string_lossy());
  }
  let installation_dir_str = installation_dir.to_string_lossy();

  if !is_in_path(&installation_dir) {
    println!("ℹ️  Add {} to PATH", installation_dir_str);
    if cfg!(windows) {
      println!("    set PATH=%PATH%;{}", installation_dir_str);
    } else {
      println!("    export PATH=\"{}:$PATH\"", installation_dir_str);
    }
  }

  Ok(())
}

fn is_in_path(dir: &Path) -> bool {
  if let Some(paths) = env::var_os("PATH") {
    for p in env::split_paths(&paths) {
      if *dir == p {
        return true;
      }
    }
  }
  false
}

#[cfg(test)]
mod tests {
  use super::*;
  use deno_core::parking_lot::Mutex;
  use std::process::Command;
  use tempfile::TempDir;
  use test_util::testdata_path;

  lazy_static::lazy_static! {
    pub static ref ENV_LOCK: Mutex<()> = Mutex::new(());
  }

  #[test]
  fn install_infer_name_from_url() {
    assert_eq!(
      infer_name_from_url(
        &Url::parse("https://example.com/abc/server.ts").unwrap()
      ),
      Some("server".to_string())
    );
    assert_eq!(
      infer_name_from_url(
        &Url::parse("https://example.com/abc/main.ts").unwrap()
      ),
      Some("abc".to_string())
    );
    assert_eq!(
      infer_name_from_url(
        &Url::parse("https://example.com/abc/mod.ts").unwrap()
      ),
      Some("abc".to_string())
    );
    assert_eq!(
      infer_name_from_url(
        &Url::parse("https://example.com/abc/index.ts").unwrap()
      ),
      Some("abc".to_string())
    );
    assert_eq!(
      infer_name_from_url(
        &Url::parse("https://example.com/abc/cli.ts").unwrap()
      ),
      Some("abc".to_string())
    );
    assert_eq!(
      infer_name_from_url(&Url::parse("https://example.com/main.ts").unwrap()),
      Some("main".to_string())
    );
    assert_eq!(
      infer_name_from_url(&Url::parse("https://example.com").unwrap()),
      None
    );
    assert_eq!(
      infer_name_from_url(&Url::parse("file:///abc/server.ts").unwrap()),
      Some("server".to_string())
    );
    assert_eq!(
      infer_name_from_url(&Url::parse("file:///abc/main.ts").unwrap()),
      Some("abc".to_string())
    );
    assert_eq!(
      infer_name_from_url(&Url::parse("file:///main.ts").unwrap()),
      Some("main".to_string())
    );
    assert_eq!(infer_name_from_url(&Url::parse("file:///").unwrap()), None);
    assert_eq!(
      infer_name_from_url(
        &Url::parse("https://example.com/abc@0.1.0").unwrap()
      ),
      Some("abc".to_string())
    );
    assert_eq!(
      infer_name_from_url(
        &Url::parse("https://example.com/abc@0.1.0/main.ts").unwrap()
      ),
      Some("abc".to_string())
    );
    assert_eq!(
      infer_name_from_url(
        &Url::parse("https://example.com/abc@def@ghi").unwrap()
      ),
      Some("abc".to_string())
    );
  }

  #[test]
  fn install_basic() {
    let _guard = ENV_LOCK.lock();
    let temp_dir = TempDir::new().expect("tempdir fail");
    let temp_dir_str = temp_dir.path().to_string_lossy().to_string();
    // NOTE: this test overrides environmental variables
    // don't add other tests in this file that mess with "HOME" and "USEPROFILE"
    // otherwise transient failures are possible because tests are run in parallel.
    // It means that other test can override env vars when this test is running.
    let original_home = env::var_os("HOME");
    let original_user_profile = env::var_os("HOME");
    let original_install_root = env::var_os("DENO_INSTALL_ROOT");
    env::set_var("HOME", &temp_dir_str);
    env::set_var("USERPROFILE", &temp_dir_str);
    env::set_var("DENO_INSTALL_ROOT", "");

    install(
      Flags::default(),
      "http://localhost:4545/echo_server.ts",
      vec![],
      Some("echo_test".to_string()),
      None,
      false,
    )
    .expect("Install failed");

    let mut file_path = temp_dir.path().join(".deno/bin/echo_test");
    assert!(file_path.exists());

    if cfg!(windows) {
      file_path = file_path.with_extension("cmd");
    }

    let content = fs::read_to_string(file_path).unwrap();
    // It's annoying when shell scripts don't have NL at the end.
    assert_eq!(content.chars().last().unwrap(), '\n');

    if cfg!(windows) {
      assert!(
        content.contains(r#""run" "http://localhost:4545/echo_server.ts""#)
      );
    } else {
      assert!(content.contains(r#"run 'http://localhost:4545/echo_server.ts'"#));
    }
    if let Some(home) = original_home {
      env::set_var("HOME", home);
    }
    if let Some(user_profile) = original_user_profile {
      env::set_var("USERPROFILE", user_profile);
    }
    if let Some(install_root) = original_install_root {
      env::set_var("DENO_INSTALL_ROOT", install_root);
    }
  }

  #[test]
  fn install_unstable() {
    let temp_dir = TempDir::new().expect("tempdir fail");
    let bin_dir = temp_dir.path().join("bin");
    std::fs::create_dir(&bin_dir).unwrap();

    install(
      Flags {
        unstable: true,
        ..Flags::default()
      },
      "http://localhost:4545/echo_server.ts",
      vec![],
      Some("echo_test".to_string()),
      Some(temp_dir.path().to_path_buf()),
      false,
    )
    .expect("Install failed");

    let mut file_path = bin_dir.join("echo_test");
    if cfg!(windows) {
      file_path = file_path.with_extension("cmd");
    }

    assert!(file_path.exists());

    let content = fs::read_to_string(file_path).unwrap();
    println!("this is the file path {:?}", content);
    if cfg!(windows) {
      assert!(content.contains(
        r#""run" "--unstable" "http://localhost:4545/echo_server.ts""#
      ));
    } else {
      assert!(content
        .contains(r#"run --unstable 'http://localhost:4545/echo_server.ts'"#));
    }
  }

  #[test]
  fn install_inferred_name() {
    let temp_dir = TempDir::new().expect("tempdir fail");
    let bin_dir = temp_dir.path().join("bin");
    std::fs::create_dir(&bin_dir).unwrap();

    install(
      Flags::default(),
      "http://localhost:4545/echo_server.ts",
      vec![],
      None,
      Some(temp_dir.path().to_path_buf()),
      false,
    )
    .expect("Install failed");

    let mut file_path = bin_dir.join("echo_server");
    if cfg!(windows) {
      file_path = file_path.with_extension("cmd");
    }

    assert!(file_path.exists());
    let content = fs::read_to_string(file_path).unwrap();
    if cfg!(windows) {
      assert!(
        content.contains(r#""run" "http://localhost:4545/echo_server.ts""#)
      );
    } else {
      assert!(content.contains(r#"run 'http://localhost:4545/echo_server.ts'"#));
    }
  }

  #[test]
  fn install_inferred_name_from_parent() {
    let temp_dir = TempDir::new().expect("tempdir fail");
    let bin_dir = temp_dir.path().join("bin");
    std::fs::create_dir(&bin_dir).unwrap();

    install(
      Flags::default(),
      "http://localhost:4545/subdir/main.ts",
      vec![],
      None,
      Some(temp_dir.path().to_path_buf()),
      false,
    )
    .expect("Install failed");

    let mut file_path = bin_dir.join("subdir");
    if cfg!(windows) {
      file_path = file_path.with_extension("cmd");
    }

    assert!(file_path.exists());
    let content = fs::read_to_string(file_path).unwrap();
    if cfg!(windows) {
      assert!(
        content.contains(r#""run" "http://localhost:4545/subdir/main.ts""#)
      );
    } else {
      assert!(content.contains(r#"run 'http://localhost:4545/subdir/main.ts'"#));
    }
  }

  #[test]
  fn install_custom_dir_option() {
    let temp_dir = TempDir::new().expect("tempdir fail");
    let bin_dir = temp_dir.path().join("bin");
    std::fs::create_dir(&bin_dir).unwrap();

    install(
      Flags::default(),
      "http://localhost:4545/echo_server.ts",
      vec![],
      Some("echo_test".to_string()),
      Some(temp_dir.path().to_path_buf()),
      false,
    )
    .expect("Install failed");

    let mut file_path = bin_dir.join("echo_test");
    if cfg!(windows) {
      file_path = file_path.with_extension("cmd");
    }

    assert!(file_path.exists());
    let content = fs::read_to_string(file_path).unwrap();
    if cfg!(windows) {
      assert!(
        content.contains(r#""run" "http://localhost:4545/echo_server.ts""#)
      );
    } else {
      assert!(content.contains(r#"run 'http://localhost:4545/echo_server.ts'"#));
    }
  }

  #[test]
  fn install_custom_dir_env_var() {
    let _guard = ENV_LOCK.lock();
    let temp_dir = TempDir::new().expect("tempdir fail");
    let bin_dir = temp_dir.path().join("bin");
    std::fs::create_dir(&bin_dir).unwrap();
    let original_install_root = env::var_os("DENO_INSTALL_ROOT");
    env::set_var("DENO_INSTALL_ROOT", temp_dir.path().to_path_buf());

    install(
      Flags::default(),
      "http://localhost:4545/echo_server.ts",
      vec![],
      Some("echo_test".to_string()),
      None,
      false,
    )
    .expect("Install failed");

    let mut file_path = bin_dir.join("echo_test");
    if cfg!(windows) {
      file_path = file_path.with_extension("cmd");
    }

    assert!(file_path.exists());
    let content = fs::read_to_string(file_path).unwrap();
    if cfg!(windows) {
      assert!(
        content.contains(r#""run" "http://localhost:4545/echo_server.ts""#)
      );
    } else {
      assert!(content.contains(r#"run 'http://localhost:4545/echo_server.ts'"#));
    }
    if let Some(install_root) = original_install_root {
      env::set_var("DENO_INSTALL_ROOT", install_root);
    }
  }

  #[test]
  fn install_with_flags() {
    let temp_dir = TempDir::new().expect("tempdir fail");
    let bin_dir = temp_dir.path().join("bin");
    std::fs::create_dir(&bin_dir).unwrap();

    install(
      Flags {
        allow_net: Some(vec![]),
        allow_read: Some(vec![]),
        no_check: true,
        log_level: Some(Level::Error),
        ..Flags::default()
      },
      "http://localhost:4545/echo_server.ts",
      vec!["--foobar".to_string()],
      Some("echo_test".to_string()),
      Some(temp_dir.path().to_path_buf()),
      false,
    )
    .expect("Install failed");

    let mut file_path = bin_dir.join("echo_test");
    if cfg!(windows) {
      file_path = file_path.with_extension("cmd");
    }

    assert!(file_path.exists());
    let content = fs::read_to_string(file_path).unwrap();
    if cfg!(windows) {
      assert!(content.contains(r#""run" "--allow-read" "--allow-net" "--quiet" "--no-check" "http://localhost:4545/echo_server.ts" "--foobar""#));
    } else {
      assert!(content.contains(r#"run --allow-read --allow-net --quiet --no-check 'http://localhost:4545/echo_server.ts' --foobar"#));
    }
  }

  #[test]
  fn install_local_module() {
    let temp_dir = TempDir::new().expect("tempdir fail");
    let bin_dir = temp_dir.path().join("bin");
    std::fs::create_dir(&bin_dir).unwrap();
    let local_module = env::current_dir().unwrap().join("echo_server.ts");
    let local_module_url = Url::from_file_path(&local_module).unwrap();
    let local_module_str = local_module.to_string_lossy();

    install(
      Flags::default(),
      &local_module_str,
      vec![],
      Some("echo_test".to_string()),
      Some(temp_dir.path().to_path_buf()),
      false,
    )
    .expect("Install failed");

    let mut file_path = bin_dir.join("echo_test");
    if cfg!(windows) {
      file_path = file_path.with_extension("cmd");
    }

    assert!(file_path.exists());
    let content = fs::read_to_string(file_path).unwrap();
    assert!(content.contains(&local_module_url.to_string()));
  }

  #[test]
  fn install_force() {
    let temp_dir = TempDir::new().expect("tempdir fail");
    let bin_dir = temp_dir.path().join("bin");
    std::fs::create_dir(&bin_dir).unwrap();

    install(
      Flags::default(),
      "http://localhost:4545/echo_server.ts",
      vec![],
      Some("echo_test".to_string()),
      Some(temp_dir.path().to_path_buf()),
      false,
    )
    .expect("Install failed");

    let mut file_path = bin_dir.join("echo_test");
    if cfg!(windows) {
      file_path = file_path.with_extension("cmd");
    }
    assert!(file_path.exists());

    // No force. Install failed.
    let no_force_result = install(
      Flags::default(),
      "http://localhost:4545/cat.ts", // using a different URL
      vec![],
      Some("echo_test".to_string()),
      Some(temp_dir.path().to_path_buf()),
      false,
    );
    assert!(no_force_result.is_err());
    assert!(no_force_result
      .unwrap_err()
      .to_string()
      .contains("Existing installation found"));
    // Assert not modified
    let file_content = fs::read_to_string(&file_path).unwrap();
    assert!(file_content.contains("echo_server.ts"));

    // Force. Install success.
    let force_result = install(
      Flags::default(),
      "http://localhost:4545/cat.ts", // using a different URL
      vec![],
      Some("echo_test".to_string()),
      Some(temp_dir.path().to_path_buf()),
      true,
    );
    assert!(force_result.is_ok());
    // Assert modified
    let file_content_2 = fs::read_to_string(&file_path).unwrap();
    assert!(file_content_2.contains("cat.ts"));
  }

  #[test]
  fn install_with_config() {
    let temp_dir = TempDir::new().expect("tempdir fail");
    let bin_dir = temp_dir.path().join("bin");
    let config_file_path = temp_dir.path().join("test_tsconfig.json");
    let config = "{}";
    let mut config_file = File::create(&config_file_path).unwrap();
    let result = config_file.write_all(config.as_bytes());
    assert!(result.is_ok());

    let result = install(
      Flags {
        config_path: Some(config_file_path.to_string_lossy().to_string()),
        ..Flags::default()
      },
      "http://localhost:4545/cat.ts",
      vec![],
      Some("echo_test".to_string()),
      Some(temp_dir.path().to_path_buf()),
      true,
    );
    eprintln!("result {:?}", result);
    assert!(result.is_ok());

    let config_file_name = "echo_test.tsconfig.json";

    let file_path = bin_dir.join(config_file_name.to_string());
    assert!(file_path.exists());
    let content = fs::read_to_string(file_path).unwrap();
    assert!(content == "{}");
  }

  // TODO: enable on Windows after fixing batch escaping
  #[cfg(not(windows))]
  #[test]
  fn install_shell_escaping() {
    let temp_dir = TempDir::new().expect("tempdir fail");
    let bin_dir = temp_dir.path().join("bin");
    std::fs::create_dir(&bin_dir).unwrap();

    install(
      Flags::default(),
      "http://localhost:4545/echo_server.ts",
      vec!["\"".to_string()],
      Some("echo_test".to_string()),
      Some(temp_dir.path().to_path_buf()),
      false,
    )
    .expect("Install failed");

    let mut file_path = bin_dir.join("echo_test");
    if cfg!(windows) {
      file_path = file_path.with_extension("cmd");
    }

    assert!(file_path.exists());
    let content = fs::read_to_string(file_path).unwrap();
    println!("{}", content);
    if cfg!(windows) {
      // TODO: see comment above this test
    } else {
      assert!(
        content.contains(r#"run 'http://localhost:4545/echo_server.ts' '"'"#)
      );
    }
  }

  // This test is disabled because it uses the `deno` binary found in `$PATH`.
  // It should use the one located in `./target/{debug|release}/`.
  #[test]
  #[ignore]
  fn install_unicode() {
    let temp_dir = TempDir::new().expect("tempdir fail");
    let bin_dir = temp_dir.path().join("bin");
    std::fs::create_dir(&bin_dir).unwrap();
    let unicode_dir = temp_dir.path().join("Magnús");
    std::fs::create_dir(&unicode_dir).unwrap();
    let local_module = unicode_dir.join("echo_server.ts");
    let local_module_str = local_module.to_string_lossy();
    std::fs::write(&local_module, "// Some JavaScript I guess").unwrap();

    install(
      Flags::default(),
      &local_module_str,
      vec![],
      Some("echo_test".to_string()),
      Some(temp_dir.path().to_path_buf()),
      false,
    )
    .expect("Install failed");

    let mut file_path = bin_dir.join("echo_test");
    if cfg!(windows) {
      file_path = file_path.with_extension("cmd");
    }

    // We need to actually run it to make sure the URL is interpreted correctly
    let status = Command::new(file_path).spawn().unwrap().wait().unwrap();
    assert!(status.success());
  }

  #[test]
  fn install_with_import_map() {
    let temp_dir = TempDir::new().expect("tempdir fail");
    let bin_dir = temp_dir.path().join("bin");
    let import_map_path = temp_dir.path().join("import_map.json");
    let import_map_url = Url::from_file_path(&import_map_path).unwrap();
    let import_map = "{ \"imports\": {} }";
    let mut import_map_file = File::create(&import_map_path).unwrap();
    let result = import_map_file.write_all(import_map.as_bytes());
    assert!(result.is_ok());

    let result = install(
      Flags {
        import_map_path: Some(import_map_path.to_string_lossy().to_string()),
        ..Flags::default()
      },
      "http://localhost:4545/cat.ts",
      vec![],
      Some("echo_test".to_string()),
      Some(temp_dir.path().to_path_buf()),
      true,
    );
    assert!(result.is_ok());

    let mut file_path = bin_dir.join("echo_test");
    if cfg!(windows) {
      file_path = file_path.with_extension("cmd");
    }
    assert!(file_path.exists());

    let mut expected_string = format!(
      "--import-map '{}' 'http://localhost:4545/cat.ts'",
      import_map_url.to_string()
    );
    if cfg!(windows) {
      expected_string = format!(
        "\"--import-map\" \"{}\" \"http://localhost:4545/cat.ts\"",
        import_map_url.to_string()
      );
    }

    let content = fs::read_to_string(file_path).unwrap();
    assert!(content.contains(&expected_string));
  }

  // Regression test for https://github.com/denoland/deno/issues/10556.
  #[test]
  fn install_file_url() {
    let temp_dir = TempDir::new().expect("tempdir fail");
    let bin_dir = temp_dir.path().join("bin");
    let module_path = fs::canonicalize(testdata_path().join("cat.ts")).unwrap();
    let file_module_string =
      Url::from_file_path(module_path).unwrap().to_string();
    assert!(file_module_string.starts_with("file:///"));

    let result = install(
      Flags::default(),
      &file_module_string,
      vec![],
      Some("echo_test".to_string()),
      Some(temp_dir.path().to_path_buf()),
      true,
    );
    assert!(result.is_ok());

    let mut file_path = bin_dir.join("echo_test");
    if cfg!(windows) {
      file_path = file_path.with_extension("cmd");
    }
    assert!(file_path.exists());

    let mut expected_string = format!("run '{}'", &file_module_string);
    if cfg!(windows) {
      expected_string = format!("\"run\" \"{}\"", &file_module_string);
    }

    let content = fs::read_to_string(file_path).unwrap();
    assert!(content.contains(&expected_string));
  }
}
