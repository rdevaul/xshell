//! `xshelld`: the per-user session daemon. Everything lives in
//! [`xshell_session::daemon`] so programs with their own tool providers can
//! build the same daemon.

fn main() -> anyhow::Result<()> {
    xshell_session::daemon::Daemon::new().main()
}
