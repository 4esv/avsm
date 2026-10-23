//! The process table, read without running anything: each process's
//! pid, parent, process group, working directory and arguments, where
//! this user may read them. `avsm doctor` finds every avsmd on a home by
//! it, and the tests find what a venture left running.

use std::path::PathBuf;

/// One process as the table shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Proc {
    pub pid: i32,
    pub ppid: i32,
    pub pgid: i32,
    /// Its working directory, when it can be read.
    pub cwd: Option<PathBuf>,
    /// Its arguments, the program first; empty when they cannot be read.
    pub args: Vec<String>,
}

/// Every process this user can see.
pub fn all() -> Vec<Proc> {
    pids().into_iter().filter_map(one).collect()
}

#[cfg(target_os = "macos")]
fn pids() -> Vec<i32> {
    // SAFETY: a null buffer asks only for the count.
    let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if n <= 0 {
        return Vec::new();
    }
    let mut buf = vec![0 as libc::pid_t; n as usize + 256];
    let size = (buf.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
    // SAFETY: buf is valid for writes of `size` bytes.
    let got = unsafe { libc::proc_listallpids(buf.as_mut_ptr().cast(), size) };
    buf.truncate(got.max(0) as usize);
    buf.retain(|p| *p > 0);
    buf
}

#[cfg(target_os = "macos")]
fn one(pid: i32) -> Option<Proc> {
    // SAFETY: proc_bsdinfo is plain data; all zeroes is a valid value.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: info is valid for writes of `size` bytes.
    let n = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    if n != size {
        return None;
    }
    // SAFETY: proc_vnodepathinfo is plain data; all zeroes is valid.
    let mut paths: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let psize = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
    // SAFETY: paths is valid for writes of `psize` bytes.
    let pn = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            (&mut paths as *mut libc::proc_vnodepathinfo).cast(),
            psize,
        )
    };
    let cwd = (pn == psize)
        .then(|| {
            // SAFETY: vip_path is a NUL-terminated C string within its array.
            let flat: &[libc::c_char] = unsafe {
                std::slice::from_raw_parts(
                    paths.pvi_cdir.vip_path.as_ptr().cast(),
                    std::mem::size_of_val(&paths.pvi_cdir.vip_path),
                )
            };
            let bytes: Vec<u8> = flat
                .iter()
                .take_while(|c| **c != 0)
                .map(|c| *c as u8)
                .collect();
            use std::os::unix::ffi::OsStringExt;
            PathBuf::from(std::ffi::OsString::from_vec(bytes))
        })
        .filter(|p| !p.as_os_str().is_empty());
    Some(Proc {
        pid,
        ppid: info.pbi_ppid as i32,
        pgid: info.pbi_pgid as i32,
        cwd,
        args: args_of(pid),
    })
}

/// A process's arguments from KERN_PROCARGS2: argc, the executable's
/// path, padding, then argc NUL-terminated arguments.
#[cfg(target_os = "macos")]
fn args_of(pid: i32) -> Vec<String> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut size: libc::size_t = 0;
    // SAFETY: a null buffer asks only for the size.
    let r = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if r != 0 || size < 4 {
        return Vec::new();
    }
    let mut buf = vec![0u8; size];
    // SAFETY: buf is valid for writes of `size` bytes.
    let r = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buf.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if r != 0 || size < 4 {
        return Vec::new();
    }
    buf.truncate(size);
    let argc = i32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]).max(0) as usize;
    let rest = &buf[4..];
    // Skip the executable's path and the NULs after it.
    let Some(end) = rest.iter().position(|b| *b == 0) else {
        return Vec::new();
    };
    let start = rest[end..]
        .iter()
        .position(|b| *b != 0)
        .map_or(rest.len(), |p| end + p);
    rest[start..]
        .split(|b| *b == 0)
        .take(argc)
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect()
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn pids() -> Vec<i32> {
    std::fs::read_dir("/proc")
        .map(|d| {
            d.filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn one(pid: i32) -> Option<Proc> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // pid (comm) state ppid pgrp ...; comm may hold anything but ends at
    // the last ')'.
    let mut rest = stat.rsplit_once(')')?.1.split_whitespace();
    let ppid = rest.nth(1)?.parse().ok()?;
    let pgid = rest.next()?.parse().ok()?;
    let args = std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|b| {
            b.split(|c| *c == 0)
                .filter(|a| !a.is_empty())
                .map(|a| String::from_utf8_lossy(a).into_owned())
                .collect()
        })
        .unwrap_or_default();
    Some(Proc {
        pid,
        ppid,
        pgid,
        cwd: std::fs::read_link(format!("/proc/{pid}/cwd")).ok(),
        args,
    })
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "android")))]
fn pids() -> Vec<i32> {
    Vec::new()
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "android")))]
fn one(_pid: i32) -> Option<Proc> {
    None
}

/// The processes whose working directory is under `dir`, or whose
/// arguments name a path under it.
pub fn under(dir: &std::path::Path) -> Vec<Proc> {
    let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let text = dir.to_string_lossy().into_owned();
    // macOS's temporary directories live under /private, and a path as
    // given may lack it.
    let short = text.strip_prefix("/private").map(str::to_string);
    let me = std::process::id() as i32;
    all()
        .into_iter()
        .filter(|p| p.pid != me)
        .filter(|p| {
            p.cwd.as_ref().is_some_and(|c| c.starts_with(&dir))
                || p.args
                    .iter()
                    .any(|a| a.contains(&text) || short.as_deref().is_some_and(|s| a.contains(s)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_holds_this_process_with_its_directory_and_arguments() {
        let me = std::process::id() as i32;
        let found = all();
        let p = found
            .iter()
            .find(|p| p.pid == me)
            .expect("this process in the table");
        // SAFETY: getppid and getpgrp have no preconditions.
        assert_eq!(p.ppid, unsafe { libc::getppid() });
        assert_eq!(p.pgid, unsafe { libc::getpgrp() });
        assert_eq!(
            p.cwd.as_ref().map(|c| std::fs::canonicalize(c).unwrap()),
            Some(std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap())
        );
        let argv0 = std::env::args().next().unwrap();
        assert_eq!(p.args.first(), Some(&argv0), "{:?}", p.args);
    }

    #[test]
    fn a_child_in_a_directory_is_found_under_it() {
        use std::os::unix::process::CommandExt;
        let tmp = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .current_dir(tmp.path())
            .process_group(0)
            .spawn()
            .unwrap();
        let found = under(tmp.path());
        let _ = child.kill();
        let _ = child.wait();
        let p = found
            .iter()
            .find(|p| p.pid == child.id() as i32)
            .unwrap_or_else(|| panic!("{found:?}"));
        assert_eq!(p.pgid, p.pid);
        assert_eq!(p.args, vec!["sleep".to_string(), "30".to_string()]);
    }
}
