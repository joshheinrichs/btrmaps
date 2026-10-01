//! Starting the scan helper as root.

use std::io::Write as _;
use std::process::{Child, Command, Stdio};

/// sudo, baked in by packagers who know where it lives, else found on PATH.
const SUDO: &str = match option_env!("SUDO") {
    Some(path) => path,
    None => "sudo",
};

/// Already root: the helper starts directly, with nothing to ask.
pub fn is_root() -> bool {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

/// Whether starting the helper needs the user's password first.
pub fn needs_password() -> bool {
    !is_root() && !quiet(Command::new(SUDO).args(["-n", "true"]))
}

fn quiet(cmd: &mut Command) -> bool {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Run `args` as root with stdin, stdout and stderr piped. With a password, sudo checks
/// it on its own first and fails at once when it is wrong; the helper then starts on
/// sudo's cached credentials, so its stdin carries only requests. Blocks while sudo
/// checks, so call it off the window's thread.
pub fn spawn(args: &[std::ffi::OsString], password: Option<&str>) -> Result<Child, String> {
    if let Some(password) = password {
        let mut check = Command::new(SUDO)
            .args(["-S", "-p", "", "-v"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("run {SUDO}: {e}"))?;
        if let Some(mut stdin) = check.stdin.take() {
            let _ = stdin.write_all(format!("{password}\n").as_bytes());
        }
        let out = check
            .wait_with_output()
            .map_err(|e| format!("run {SUDO}: {e}"))?;
        let said = String::from_utf8_lossy(&out.stderr);
        match (out.status.success(), said.contains("incorrect password")) {
            (true, _) => {}
            (false, true) => return Err("Wrong password, try again.".into()),
            (false, false) => return Err(said.trim().to_string()),
        }
    }
    let mut cmd = match is_root() {
        true => Command::new(&args[0]),
        false => {
            let mut sudo = Command::new(SUDO);
            sudo.args(["-n", "--"]).arg(&args[0]);
            sudo
        }
    };
    cmd.args(&args[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("start the scan: {e}"))
}
