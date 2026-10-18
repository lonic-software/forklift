//! `raise_open_file_limit` changes a process-wide limit, so its test runs in a binary of its own:
//! lowering the limit beside other tests running in parallel threads could starve them. Unix only:
//! elsewhere there is no such limit.
#![cfg(unix)]

use forklift_core::util::lock_utils::raise_open_file_limit;

/// A raise never lowers a limit already above anything it would try, and a low soft open-file limit
/// is raised. The test lowers it to 1024 for the second half, so the raise has something to do
/// whatever limit the test inherits.
#[test]
fn a_low_open_file_limit_is_raised() {
    let read = || {
        let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) }, 0);
        limit
    };
    let inherited = read();
    let reported = raise_open_file_limit();
    let original = read();
    assert_eq!(reported, Some(u64::from(original.rlim_cur)), "the raise reports the limit in force");
    assert!(original.rlim_cur >= inherited.rlim_cur, "a raise never lowers the limit");

    let low = original.rlim_cur.min(1024);
    if original.rlim_max <= low {
        return; // A hard limit this low leaves nothing to raise.
    }
    let mut lowered = original;
    lowered.rlim_cur = low;
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lowered) }, 0);

    let reported = raise_open_file_limit();

    let raised = read().rlim_cur;
    assert!(raised > low, "the soft limit was raised above {}: it is {}", low, raised);
    assert_eq!(reported, Some(u64::from(raised)), "the raise reports the limit now in force");
}
