use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::Path;

pub(super) fn acquire(data_directory: &Path) -> io::Result<Option<File>> {
    fs::create_dir_all(data_directory)?;
    // Keep the file after dropping the lock so subsequent contenders always
    // use the same file. The OS releases the lock when the process exits.
    let lock_file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(data_directory.join("instance.lock"))?;
    match lock_file.try_lock() {
        Ok(()) => Ok(Some(lock_file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(error)) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{env, path::PathBuf, process::Command};

    const CHILD_DIRECTORY: &str = "MACTOWINDOWS_TEST_INSTANCE_LOCK_DIRECTORY";
    const CHILD_EXPECTS_LOCK: &str = "MACTOWINDOWS_TEST_INSTANCE_EXPECTS_LOCK";

    #[test]
    fn subprocess_lock_attempt() {
        let Some(directory) = env::var_os(CHILD_DIRECTORY).map(PathBuf::from) else {
            return;
        };
        let expects_lock = env::var_os(CHILD_EXPECTS_LOCK).is_some();
        assert_eq!(acquire(&directory).unwrap().is_some(), expects_lock);
    }

    fn check_in_another_process(directory: &Path, expects_lock: bool) {
        let mut command = Command::new(env::current_exe().unwrap());
        command
            .args(["--exact", "instance_lock::tests::subprocess_lock_attempt"])
            .env(CHILD_DIRECTORY, directory)
            .env_remove(CHILD_EXPECTS_LOCK);
        if expects_lock {
            command.env(CHILD_EXPECTS_LOCK, "1");
        }
        let output = command.output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success(), "{stdout}");
        assert!(stdout.contains("1 passed"), "{stdout}");
    }

    #[test]
    fn shared_directory_allows_one_live_process() {
        let shared = tempfile::tempdir().unwrap();
        let independent = tempfile::tempdir().unwrap();
        let primary = acquire(shared.path()).unwrap().unwrap();
        check_in_another_process(shared.path(), false);
        check_in_another_process(independent.path(), true);
        drop(primary);
        check_in_another_process(shared.path(), true);
    }
}
