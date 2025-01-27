// Copyright 2021 Ross Light
// Copyright 2010-2018 Avery Pennarun and contributors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// SPDX-License-Identifier: Apache-2.0

use common_path;
use std::borrow::Cow;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt::{self, Display, Formatter};
use std::fs;
use std::iter;
use std::os::unix::fs as unixfs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::str::FromStr;
use tempfile::{self, TempDir};

use super::error::{RedoError, RedoErrorKind};
use super::helpers::{self, RedoPath, RedoPathBuf};

const ENV_BASE: &str = "REDO_BASE";
pub const ENV_COLOR: &str = "REDO_COLOR";
pub const ENV_DEBUG: &str = "REDO_DEBUG";
pub const ENV_DEBUG_LOCKS: &str = "REDO_DEBUG_LOCKS";
pub const ENV_DEBUG_PIDS: &str = "REDO_DEBUG_PIDS";
pub(crate) const ENV_DEPTH: &str = "REDO_DEPTH";
pub const ENV_KEEP_GOING: &str = "REDO_KEEP_GOING";
const ENV_LOCKS_BROKEN: &str = "REDO_LOCKS_BROKEN";
pub const ENV_LOG: &str = "REDO_LOG";
pub(crate) const ENV_LOG_INODE: &str = "REDO_LOG_INODE";
pub const ENV_NO_OOB: &str = "REDO_NO_OOB";
pub const ENV_PRETTY: &str = "REDO_PRETTY";
pub(crate) const ENV_PWD: &str = "REDO_PWD";
const ENV_REDO: &str = "REDO";
const ENV_RUNID: &str = "REDO_RUNID";
pub const ENV_SHUFFLE: &str = "REDO_SHUFFLE";
const ENV_STARTDIR: &str = "REDO_STARTDIR";
pub(crate) const ENV_TARGET: &str = "REDO_TARGET";
pub const ENV_UNLOCKED: &str = "REDO_UNLOCKED";
pub const ENV_VERBOSE: &str = "REDO_VERBOSE";
pub const ENV_XTRACE: &str = "REDO_XTRACE";

#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Env {
    is_toplevel: bool,
    base: PathBuf,
    pub(crate) pwd: PathBuf,
    target: RedoPathBuf,
    depth: String,
    pub(crate) debug: i32,
    debug_locks: bool,
    debug_pids: bool,
    locks_broken: bool,
    pub(crate) verbose: i32,
    pub(crate) xtrace: i32,
    pub(crate) keep_going: bool,
    log: i32,
    log_inode: OsString,
    color: i32,
    pretty: i32,
    pub(crate) shuffle: bool,
    pub(crate) startdir: PathBuf,
    pub(crate) runid: Option<i64>,
    pub(crate) unlocked: bool,
    pub(crate) no_oob: bool,

    redo_links_dir: Option<Rc<TempDir>>,
}

impl Env {
    /// Start a session (if needed) for a command that does need the state db.
    pub fn init<P: AsRef<RedoPath>>(targets: &[P]) -> Result<Env, RedoError> {
        let mut is_toplevel = false;
        let mut redo_links_dir = None;
        if !get_bool(ENV_REDO) {
            is_toplevel = true;
            let exe_path = env::current_exe().map_err(RedoError::opaque_error)?;
            let exe_names = [
                &exe_path,
                &fs::canonicalize(&exe_path).map_err(RedoError::opaque_error)?,
            ];
            let dir_names: Vec<&Path> = exe_names.iter().filter_map(|&p| p.parent()).collect();
            let mut try_names: Vec<Cow<Path>> = Vec::new();
            try_names.extend(dir_names.iter().map(|&p| {
                let mut p2 = PathBuf::from(p);
                p2.extend(["..", "lib", "redo"].iter());
                Cow::Owned(p2)
            }));
            try_names.extend(dir_names.iter().map(|&p| {
                let mut p2 = PathBuf::from(p);
                p2.extend(["..", "redo"].iter());
                Cow::Owned(p2)
            }));
            try_names.extend(dir_names.iter().map(|&p| Cow::Borrowed(p)));

            let mut dirs: Vec<Cow<Path>> = Vec::new();
            let mut found_unlocked = false;
            for k in try_names {
                if !found_unlocked && k.join("redo-unlocked").exists() {
                    found_unlocked = true;
                }
                if !dirs.iter().any(|k2| k2 == &k) {
                    dirs.push(k);
                }
            }
            if !found_unlocked {
                let d = Env::make_redo_links_dir(&exe_path)?;
                dirs.push(Cow::Owned(d.path().to_path_buf()));
                redo_links_dir = Some(Rc::new(d));
            }
            let old_path = env::var_os("PATH").unwrap_or_default();
            let mut new_path = OsString::new();
            for p in dirs {
                new_path.push(p.as_os_str());
                new_path.push(":");
            }
            new_path.push(old_path);
            env::set_var("PATH", new_path);
            env::set_var(ENV_REDO, exe_path);
        }
        if !get_bool(ENV_BASE) {
            let targets: Vec<&RedoPath> = if targets.is_empty() {
                // If no other targets given, assume the current directory.
                vec![unsafe { RedoPath::from_str_unchecked("all") }]
            } else {
                targets.iter().map(AsRef::as_ref).collect()
            };
            let cwd = env::current_dir().map_err(RedoError::opaque_error)?;
            let mut dirs: Vec<PathBuf> = Vec::with_capacity(targets.len());
            for t in targets.iter() {
                match t.as_path().parent() {
                    Some(par) => dirs.push(helpers::abs_path(&cwd, &par).into_owned()),
                    None => {
                        return Err(
                            RedoErrorKind::InvalidTarget(t.as_os_str().to_os_string()).into()
                        )
                    }
                }
            }
            let orig_base = common_path::common_path_all(
                dirs.iter()
                    .map(|p| p as &Path)
                    .chain(iter::once(cwd.as_ref())),
            )
            .unwrap();
            let mut base = Some(orig_base.clone());
            while let Some(mut b) = base {
                b.push(".redo");
                let exists = b.exists();
                b.pop(); // .redo
                if exists {
                    base = Some(b);
                    break;
                }
                base = if b.pop() {
                    // up to parent
                    Some(b)
                } else {
                    None
                };
            }
            env::set_var(ENV_BASE, base.unwrap_or(orig_base));
            env::set_var(ENV_STARTDIR, cwd);
        }
        Ok(Env {
            is_toplevel,
            redo_links_dir,
            ..Env::inherit()?
        })
    }

    fn make_redo_links_dir(exe_path: &Path) -> Result<TempDir, RedoError> {
        let d = tempfile::tempdir().map_err(RedoError::opaque_error)?;
        const BINARIES: &[&str] = &[
            "redo",
            "redo-always",
            "redo-ifchange",
            "redo-ifcreate",
            "redo-log",
            "redo-ood",
            "redo-sources",
            "redo-stamp",
            "redo-targets",
            "redo-unlocked",
            "redo-whichdo",
        ];
        let mut path = d.path().to_path_buf();
        for name in BINARIES {
            path.push(name);
            unixfs::symlink(exe_path, &path).map_err(RedoError::opaque_error)?;
            path.pop();
        }
        Ok(d)
    }

    /// Start a session (if needed) for a command that needs no state db.
    pub fn init_no_state() -> Result<Env, RedoError> {
        let mut is_toplevel = false;
        if !get_bool(ENV_REDO) {
            env::set_var(ENV_REDO, "NOT_DEFINED");
            is_toplevel = true;
        }
        if !get_bool(ENV_BASE) {
            env::set_var(ENV_BASE, "NOT_DEFINED");
        }
        Ok(Env {
            is_toplevel,
            ..Env::inherit()?
        })
    }

    /// Read environment (which must already be set) to get runtime settings.
    pub fn inherit() -> Result<Env, RedoError> {
        use std::convert::TryFrom;

        if !get_bool(ENV_REDO) {
            return Err(RedoError::new(format!("must be run from inside a .do")));
        }
        let v = Env {
            is_toplevel: false,
            base: env::var_os(ENV_BASE).unwrap_or_default().into(),
            pwd: env::var_os(ENV_PWD).unwrap_or_default().into(),
            target: RedoPathBuf::try_from(env::var_os(ENV_TARGET).unwrap_or_default()).map_err(
                |e| {
                    RedoError::new(format!("{}: {}", ENV_TARGET, e))
                        .with_kind(RedoErrorKind::InvalidTarget(e.input().to_os_string()))
                },
            )?,
            depth: env::var(ENV_DEPTH).unwrap_or_default(),
            debug: get_int(ENV_DEBUG, 0) as i32,
            debug_locks: get_bool(ENV_DEBUG_LOCKS),
            debug_pids: get_bool(ENV_DEBUG_PIDS),
            locks_broken: get_bool(ENV_LOCKS_BROKEN),
            verbose: get_int(ENV_VERBOSE, 0) as i32,
            xtrace: get_int(ENV_XTRACE, 0) as i32,
            keep_going: get_bool(ENV_KEEP_GOING),
            log: get_int(ENV_LOG, 1) as i32,
            log_inode: env::var_os(ENV_LOG_INODE).unwrap_or_default(),
            color: get_int(ENV_COLOR, 0) as i32,
            pretty: get_int(ENV_PRETTY, 0) as i32,
            shuffle: get_bool(ENV_SHUFFLE),
            startdir: env::var_os(ENV_STARTDIR).unwrap_or_default().into(),
            runid: match get_int(ENV_RUNID, 0) {
                0 => None,
                x => Some(x),
            },
            unlocked: get_bool(ENV_UNLOCKED),
            no_oob: get_bool(ENV_NO_OOB),
            redo_links_dir: None,
        };
        if v.depth.contains(|c| c != ' ') {
            return Err(RedoError::new(format!(
                "{}={:?} contains non-space characters",
                ENV_DEPTH, &v.depth
            )));
        }
        // not inheritable by subprocesses
        env::set_var(ENV_UNLOCKED, "");
        env::set_var(ENV_NO_OOB, "");
        Ok(v)
    }

    #[inline]
    pub fn is_toplevel(&self) -> bool {
        self.is_toplevel
    }

    /// Absolute path of the directory that contains (or should contain)
    /// the .redo directory.
    #[inline]
    pub fn base(&self) -> &Path {
        &self.base
    }

    #[inline]
    pub fn pwd(&self) -> &Path {
        &self.pwd
    }

    #[inline]
    pub fn target(&self) -> &RedoPath {
        &self.target
    }

    /// Indent depth of the logs for this process as a string of the appropriate
    /// number of space characters.
    #[inline]
    pub fn depth(&self) -> &str {
        &self.depth
    }

    /// Whether to print messages about file locking (useful for debugging).
    #[inline]
    pub fn debug_locks(&self) -> bool {
        self.debug_locks
    }

    #[inline]
    pub fn set_debug_locks(&mut self, val: bool) {
        self.debug_locks = val;
    }

    /// Whether to print process ids as part of log messages (useful for debugging).
    #[inline]
    pub fn debug_pids(&self) -> bool {
        self.debug_pids
    }

    #[inline]
    pub fn set_debug_pids(&mut self, val: bool) {
        self.debug_pids = val;
    }

    #[inline]
    pub fn locks_broken(&self) -> bool {
        self.locks_broken
    }

    #[inline]
    pub fn log(&self) -> OptionalBool {
        if self.log == 0 {
            OptionalBool::Off
        } else if self.log == 1 {
            OptionalBool::Auto
        } else {
            OptionalBool::On
        }
    }

    #[inline]
    pub fn log_inode(&self) -> &OsStr {
        &self.log_inode
    }

    #[inline]
    pub fn color(&self) -> OptionalBool {
        if self.color == 0 {
            OptionalBool::Off
        } else if self.color == 1 {
            OptionalBool::Auto
        } else {
            OptionalBool::On
        }
    }

    #[inline]
    pub fn pretty(&self) -> OptionalBool {
        if self.pretty == 0 {
            OptionalBool::Off
        } else if self.pretty == 1 {
            OptionalBool::Auto
        } else {
            OptionalBool::On
        }
    }

    #[inline]
    pub fn startdir(&self) -> &Path {
        &self.startdir
    }

    #[inline]
    pub fn is_unlocked(&self) -> bool {
        self.unlocked
    }

    /// If file locking is broken, update the environment accordingly.
    pub(crate) fn mark_locks_broken(&mut self) {
        env::set_var(ENV_LOCKS_BROKEN, "1");
        // FIXME: redo-log doesn't work when fcntl locks are broken.
        // We can probably work around that someday.
        env::set_var(ENV_LOG, "0");

        self.locks_broken = true;
        self.log = 0;
    }

    pub(crate) fn fill_runid(&mut self, runid: i64) {
        assert!(self.runid.is_none());
        self.runid = Some(runid);
        env::set_var(ENV_RUNID, runid.to_string());
    }
}

fn get_int<K: AsRef<OsStr>>(key: K, default: i64) -> i64 {
    env::var(key)
        .ok()
        .and_then(|v| i64::from_str(&v).ok())
        .unwrap_or(default)
}

fn get_bool<K: AsRef<OsStr>>(key: K) -> bool {
    env::var_os(key).map_or(false, |v| !v.is_empty())
}

/// A tri-state value that is forced on or off, or has an automatic (default) value.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
#[repr(u8)]
pub enum OptionalBool {
    Off = 0,
    Auto = 1,
    On = 2,
}

impl OptionalBool {
    /// Returns the boolean value or a provided default.
    #[inline]
    pub fn unwrap_or(self, default: bool) -> bool {
        match self {
            OptionalBool::On => true,
            OptionalBool::Off => false,
            OptionalBool::Auto => default,
        }
    }

    /// Returns the boolean value or computes it from a closure.
    #[inline]
    pub fn unwrap_or_else<F: FnOnce() -> bool>(self, f: F) -> bool {
        match self {
            OptionalBool::On => true,
            OptionalBool::Off => false,
            OptionalBool::Auto => f(),
        }
    }
}

impl Default for OptionalBool {
    #[inline]
    fn default() -> OptionalBool {
        OptionalBool::Auto
    }
}

impl Display for OptionalBool {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            OptionalBool::Off => f.write_str("false"),
            OptionalBool::Auto => f.write_str("auto"),
            OptionalBool::On => f.write_str("true"),
        }
    }
}

impl From<Option<bool>> for OptionalBool {
    fn from(ob: Option<bool>) -> OptionalBool {
        match ob {
            Some(true) => OptionalBool::On,
            Some(false) => OptionalBool::Off,
            None => OptionalBool::Auto,
        }
    }
}

impl From<OptionalBool> for Option<bool> {
    fn from(ob: OptionalBool) -> Option<bool> {
        match ob {
            OptionalBool::On => Some(true),
            OptionalBool::Off => Some(false),
            OptionalBool::Auto => None,
        }
    }
}
