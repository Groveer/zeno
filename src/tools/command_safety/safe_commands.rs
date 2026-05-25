//! Per-command argument-level safety checks.
//!
//! Inspired by Codex's `is_safe_command` / `is_dangerous_command` design.
//! Each known command gets its own checker that validates arguments for
//! safe (read-only) usage only.

/// Check whether a command word list is safe (read-only) based on the
/// command name and its arguments.
///
/// Returns `Some(true)` if definitely safe, `Some(false)` if definitely
/// not safe, and `None` if the command is not recognized (caller should
/// fall back to general heuristics).
pub fn check_command_safe(words: &[String]) -> Option<bool> {
    let cmd0 = words.first()?;

    // Canonicalize: treat zsh as bash
    let cmd = if cmd0 == "zsh" { "bash" } else { cmd0.as_str() };

    match cmd {
        // Pure read-only commands — no arguments can make them destructive
        "cat" | "cd" | "cut" | "echo" | "expr" | "false" | "head" | "id" | "ls" | "nl"
        | "paste" | "pwd" | "rev" | "seq" | "stat" | "tail" | "tr" | "true" | "uname" | "uniq"
        | "wc" | "which" | "whoami" | "hostname" | "printf" | "printenv" | "locate" | "ag"
        | "ack" | "type" | "more" | "less" | "file" | "sort" => Some(true),

        // `base64` is safe unless `--output` is used
        "base64" => Some(check_base64_safe(words)),

        // `find` and `fd` are safe unless exec/delete flags are used
        "find" => Some(check_find_safe(words)),
        "fd" => Some(check_find_safe(words)), // fd has -x/--exec flags like find

        // `rg` / `ripgrep` is safe unless `--pre` or `-z` is used
        "rg" | "ripgrep" | "rga" => Some(check_rg_safe(words)),

        // `sed` is only safe with `-n {N}p` (no side effects)
        "sed" => Some(check_sed_safe(words)),

        // `grep` is always safe (read-only by nature)
        "grep" | "egrep" | "fgrep" => Some(true),

        // `git` depends on subcommand + arguments
        "git" => Some(check_git_safe(words)),

        // `xargs` is safe only with `-n` (no-exec mode)
        "xargs" => Some(check_xargs_safe(words)),

        // `awk` is NOT safe — can execute shell commands (`system()`),
        // write files (`print > file`), and open network connections.
        // `awk '{system("rm -rf /")}'` is undetectable via arguments.
        // We conservatively treat it as unknown (delegate to sandbox).
        // (Removed from match — falls through to `_ => None`)

        // `sudo` / `doas` — recurse into the inner command
        "sudo" | "doas" => {
            if words.len() >= 2 {
                check_command_safe(&words[1..])
            } else {
                Some(false)
            }
        }

        // Unknown command — can't determine safety
        _ => None,
    }
}

/// Check whether a command word list is known to be dangerous.
///
/// Returns `Some(true)` if definitely dangerous, `Some(false)` if not,
/// and `None` if the command is not recognized.
pub fn check_command_dangerous(words: &[String]) -> Option<bool> {
    let cmd0 = words.first()?;
    let cmd = if cmd0 == "zsh" { "bash" } else { cmd0.as_str() };

    match cmd {
        // `rm -rf` or `rm -f` is dangerous
        "rm" => {
            let has_force = words.iter().skip(1).any(|a| {
                a == "-f"
                    || a == "-rf"
                    || a == "-fr"
                    || a == "--force"
                    || a == "--recursive"
                    || a == "-R"
                    || a == "-r"
                    // Combined short flags like -Rf, -fR, -rRf, etc.
                    || (a.starts_with('-') && a.len() > 2
                        && !a.starts_with("--")
                        && (a.contains('f') || a.contains('R') || a.contains('r')))
            });
            Some(has_force)
        }

        // `sudo` — recurse
        "sudo" | "doas" => {
            if words.len() >= 2 {
                check_command_dangerous(&words[1..])
            } else {
                None
            }
        }

        // `dd` is always dangerous (can overwrite disks)
        "dd" => Some(true),

        // `mkfs.*` is always dangerous
        cmd if cmd.starts_with("mkfs") => Some(true),

        _ => None,
    }
}

// ── Per-command checkers ─────────────────────────────────────────────

fn check_base64_safe(words: &[String]) -> bool {
    const UNSAFE_BASE64_OPTIONS: &[&str] = &["-o", "--output"];
    !words.iter().skip(1).any(|arg| {
        UNSAFE_BASE64_OPTIONS.contains(&arg.as_str())
            || arg.starts_with("--output=")
            || (arg.starts_with("-o") && arg != "-o")
    })
}

fn check_find_safe(words: &[String]) -> bool {
    const UNSAFE_FIND_OPTIONS: &[&str] = &[
        "-exec", "-execdir", "-ok", "-okdir", "-delete", "-fls", "-fprint", "-fprint0", "-fprintf",
    ];
    // Also reject `--exec` (long form for both find and fd)
    !words.iter().any(|arg| {
        let a = arg.as_str();
        UNSAFE_FIND_OPTIONS.contains(&a)
            || a == "--exec"
            // `-x` is fd's short form for --exec.
            // Short flags can be combined (e.g. `-xR`), so we must check
            // starts_with rather than exact match. `-x` is len=2, combined
            // forms like `-xR` are len>2. This catches both exact and
            // combined forms without false positives on `--exec` or `-xargs`.
            || (a.starts_with("-x") && a.len() > 2)
            || a == "-x"
    })
}

fn check_rg_safe(words: &[String]) -> bool {
    const UNSAFE_RG_OPTIONS: &[&str] = &["--pre", "--hostname-bin", "--search-zip"];
    !words.iter().any(|arg| {
        let a = arg.as_str();
        UNSAFE_RG_OPTIONS.contains(&a)
            || a.starts_with("--pre=")
            || a.starts_with("--hostname-bin=")
            // `-z` enables search in compressed files — combined with `--pre`
            // this can execute arbitrary decompression commands. Short flags
            // can be combined (e.g. `-zr`), so catch both exact and combined forms.
            || a == "-z"
            || (a.starts_with("-z") && a.len() > 2)
    })
}

fn check_sed_safe(words: &[String]) -> bool {
    // Only sed -n with a range like `1,5p` or `10p` is safe.
    // We check the arguments after the command name, skipping `-e` if present
    // (e.g. `sed -n -e 1,5p file.txt`).
    //
    // After finding a valid script, we scan remaining args for `-i`/`--in-place`
    // because `-i` makes even a -n script destructive (in-place edit).
    let mut i = 1;
    let mut found_n = false;

    while i < words.len() {
        let w = words[i].as_str();
        match w {
            "-n" => {
                found_n = true;
                i += 1;
            }
            "-e" => {
                i += 1;
                if !found_n {
                    return false; // -e without -n is not safe
                }
                // The next arg must be a valid sed-n script
                if i < words.len() && is_valid_sed_n_arg(Some(words[i].as_str())) {
                    i += 1;
                    break; // script found, scan remaining for -i
                }
                return false;
            }
            _ => {
                if !found_n {
                    return false; // positional before -n
                }
                if is_valid_sed_n_arg(Some(w)) {
                    i += 1;
                    break; // script found, scan remaining for -i
                }
                return false; // not a valid script argument
            }
        }
    }

    // After finding a valid script, check remaining args for destructive flags
    while i < words.len() {
        if words[i] == "-i" || words[i] == "--in-place" {
            return false; // in-place edit is destructive
        }
        i += 1;
    }

    found_n
}

fn is_valid_sed_n_arg(arg: Option<&str>) -> bool {
    let s = match arg {
        Some(s) => s,
        None => return false,
    };
    let core = match s.strip_suffix('p') {
        Some(rest) => rest,
        None => return false,
    };
    let parts: Vec<&str> = core.split(',').collect();
    match parts.as_slice() {
        [num] => !num.is_empty() && num.chars().all(|c| c.is_ascii_digit()),
        [a, b] => {
            !a.is_empty()
                && !b.is_empty()
                && a.chars().all(|c| c.is_ascii_digit())
                && b.chars().all(|c| c.is_ascii_digit())
        }
        _ => false,
    }
}

fn check_git_safe(words: &[String]) -> bool {
    // Allow only read-only subcommands: status, log, diff, show, branch (read-only flags)
    let subcommand_idx = find_git_subcommand(words, &["status", "log", "diff", "show", "branch"]);
    let Some((subcommand_idx, subcommand)) = subcommand_idx else {
        return false;
    };

    let global_args = &words[1..subcommand_idx];
    if git_has_unsafe_global_option(global_args) {
        return false;
    }

    let subcommand_args = &words[subcommand_idx + 1..];
    match subcommand {
        "status" | "log" | "diff" | "show" => git_subcommand_args_are_read_only(subcommand_args),
        "branch" => {
            git_subcommand_args_are_read_only(subcommand_args)
                && git_branch_is_read_only(subcommand_args)
        }
        _ => false,
    }
}

fn find_git_subcommand<'a>(
    command: &'a [String],
    subcommands: &[&str],
) -> Option<(usize, &'a str)> {
    let cmd0 = command.first()?;
    if cmd0 != "git" {
        return None;
    }

    let mut skip_next = false;
    for (idx, arg) in command.iter().enumerate().skip(1) {
        if skip_next {
            skip_next = false;
            continue;
        }

        let arg = arg.as_str();

        if is_git_global_option_with_inline_value(arg) {
            continue;
        }
        if is_git_global_option_with_value(arg) {
            skip_next = true;
            continue;
        }
        if arg == "--" || arg.starts_with('-') {
            continue;
        }
        if subcommands.contains(&arg) {
            return Some((idx, arg));
        }
        // First non-option token determines the subcommand
        return None;
    }
    None
}

fn is_git_global_option_with_value(arg: &str) -> bool {
    matches!(
        arg,
        "-C" | "-c"
            | "--config-env"
            | "--exec-path"
            | "--git-dir"
            | "--namespace"
            | "--super-prefix"
            | "--work-tree"
    )
}

fn is_git_global_option_with_inline_value(arg: &str) -> bool {
    arg.starts_with("--config-env=")
        || arg.starts_with("--exec-path=")
        || arg.starts_with("--git-dir=")
        || arg.starts_with("--namespace=")
        || arg.starts_with("--super-prefix=")
        || arg.starts_with("--work-tree=")
        || ((arg.starts_with("-C") || arg.starts_with("-c")) && arg.len() > 2)
}

const UNSAFE_GIT_SUBCOMMAND_OPTIONS: &[&str] = &["--output", "--ext-diff", "--textconv", "--exec"];

fn git_subcommand_args_are_read_only(args: &[String]) -> bool {
    !args.iter().any(|arg| {
        let arg = arg.as_str();
        UNSAFE_GIT_SUBCOMMAND_OPTIONS.contains(&arg)
            || arg.starts_with("--output=")
            || arg.starts_with("--exec=")
    })
}

fn git_has_unsafe_global_option(global_args: &[String]) -> bool {
    global_args.iter().map(String::as_str).any(|arg| {
        // `--paginate` / `-p` forces output through $PAGER, which could
        // execute arbitrary commands through a custom pager configuration.
        // These are boolean flags, not value-taking options, so they must
        // NOT go through is_git_global_option_with_value (which would cause
        // find_git_subcommand to skip the next token).
        if arg == "--paginate" || arg == "-p" {
            return true;
        }

        let mat =
            is_git_global_option_with_value(arg) || is_git_global_option_with_inline_value(arg);
        // -C is safe — just changes working directory (e.g. `git -C /repo status`)
        let is_c_option =
            arg == "-C" || (arg.starts_with("-C") && !arg.starts_with("-c") && arg.len() > 2);
        mat && !is_c_option
    })
}

fn git_branch_is_read_only(args: &[String]) -> bool {
    if args.is_empty() {
        return true; // bare `git branch` — lists branches
    }
    let mut saw_read_only_flag = false;
    for arg in args.iter().map(String::as_str) {
        match arg {
            "--list" | "-l" | "--show-current" | "-a" | "--all" | "-r" | "--remotes" | "-v"
            | "-vv" | "--verbose" | "--merged" | "--no-merged" | "--contains" | "--no-contains"
            | "--edit-description" | "--column" | "--sort" => {
                saw_read_only_flag = true;
            }
            _ if arg.starts_with("--format=")
                || arg.starts_with("--sort=")
                || arg.starts_with("--column=")
                || arg.starts_with("--merged=")
                || arg.starts_with("--no-merged=")
                || arg.starts_with("--contains=")
                || arg.starts_with("--no-contains=") =>
            {
                saw_read_only_flag = true;
            }
            _ if saw_read_only_flag => {
                // Positional arg after a read-only flag is a branch-name pattern
                // (e.g. `git branch --list my-feature` → lists matching branches)
                continue;
            }
            _ => return false, // positional arg without read-only flag = creating a branch
        }
    }
    saw_read_only_flag
}

fn check_xargs_safe(words: &[String]) -> bool {
    // `xargs` itself is dangerous — it executes commands.
    // We only allow `xargs -n<N>` with NO additional command argument
    // (defaults to `echo`), or with a trailing command that is itself safe.
    // Bare `xargs` or `xargs <cmd>` must not be auto-approved.
    let args = &words[1..];
    if args.is_empty() {
        return false; // bare `xargs` without flags or command is unsafe
    }

    // Check if there's a -n flag
    let has_n = args.iter().any(|a| a.starts_with("-n"));
    if !has_n {
        return false; // no -n flag, unsafe
    }

    // Get the command after all flags
    let cmd_start = args
        .iter()
        .position(|a| !a.starts_with('-'))
        .unwrap_or(args.len());
    let trailing_cmd = &args[cmd_start..];

    if trailing_cmd.is_empty() {
        // `xargs -n<N>` with no explicit command defaults to `echo` — safe
        return true;
    }

    // If there's a trailing command, only allow single-word safe commands
    if trailing_cmd.len() != 1 {
        return false;
    }
    matches!(
        trailing_cmd[0].as_str(),
        "echo" | "printf" | "ls" | "cat" | "head" | "tail" | "wc"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn test_read_only_commands() {
        assert_eq!(check_command_safe(&v(&["ls"])), Some(true));
        assert_eq!(check_command_safe(&v(&["cat", "file.txt"])), Some(true));
        assert_eq!(check_command_safe(&v(&["echo", "hello"])), Some(true));
        assert_eq!(check_command_safe(&v(&["pwd"])), Some(true));
        assert_eq!(check_command_safe(&v(&["whoami"])), Some(true));
        assert_eq!(check_command_safe(&v(&["git", "status"])), Some(true));
    }

    #[test]
    fn test_find_safety() {
        assert_eq!(
            check_command_safe(&v(&["find", ".", "-name", "*.rs"])),
            Some(true)
        );
        assert_eq!(
            check_command_safe(&v(&["find", ".", "-exec", "rm", "{}", ";"])),
            Some(false)
        );
        assert_eq!(
            check_command_safe(&v(&["find", ".", "-delete"])),
            Some(false)
        );
    }

    #[test]
    fn test_fd_combined_flags() {
        // fd -x (short for --exec) must be caught in all forms
        assert_eq!(
            check_command_safe(&v(&["fd", "-x", "echo", "{}"])),
            Some(false)
        );
        // Combined short flags: -xR means -x + -R, tree-sitter parses as one word
        assert_eq!(
            check_command_safe(&v(&["fd", "-xR", "echo", "{}"])),
            Some(false)
        );
    }

    #[test]
    fn test_rg_combined_flags() {
        // rg -z must be caught in all forms
        assert_eq!(
            check_command_safe(&v(&["rg", "-z", "pattern"])),
            Some(false)
        );
        // Combined short flags: -zr means -z + -r
        assert_eq!(
            check_command_safe(&v(&["rg", "-zr", "pattern"])),
            Some(false)
        );
    }

    #[test]
    fn test_git_safety() {
        assert_eq!(check_command_safe(&v(&["git", "status"])), Some(true));
        assert_eq!(
            check_command_safe(&v(&["git", "log", "-n", "5"])),
            Some(true)
        );
        assert_eq!(
            check_command_safe(&v(&["git", "diff", "--cached"])),
            Some(true)
        );
        assert_eq!(
            check_command_safe(&v(&["git", "branch", "--show-current"])),
            Some(true)
        );
        assert_eq!(
            check_command_safe(&v(&["git", "branch", "new-branch"])),
            Some(false)
        );
        assert_eq!(check_command_safe(&v(&["git", "push"])), Some(false));
        assert_eq!(
            check_command_safe(&v(&["git", "log", "--output=/tmp/out"])),
            Some(false)
        );
        assert_eq!(
            check_command_safe(&v(&["git", "-C", ".", "status"])),
            Some(true)
        );
        assert_eq!(
            check_command_safe(&v(&["git", "checkout", "main"])),
            Some(false)
        );
        // `-p` / `--paginate` are unsafe — forces output through $PAGER
        assert_eq!(
            check_command_safe(&v(&["git", "-p", "status"])),
            Some(false)
        );
        assert_eq!(
            check_command_safe(&v(&["git", "--paginate", "log", "-n", "5"])),
            Some(false)
        );
        // -p after subcommand is `--patch` (read-only), not `--paginate`
        assert_eq!(check_command_safe(&v(&["git", "status", "-p"])), Some(true));
        assert_eq!(check_command_safe(&v(&["git", "diff", "-p"])), Some(true));
    }

    #[test]
    fn test_rg_safety() {
        assert_eq!(
            check_command_safe(&v(&["rg", "pattern", "src/"])),
            Some(true)
        );
        assert_eq!(
            check_command_safe(&v(&["rg", "--pre", "echo", "pattern"])),
            Some(false)
        );
        assert_eq!(
            check_command_safe(&v(&["rg", "-z", "pattern"])),
            Some(false)
        );
    }

    #[test]
    fn test_sed_safety() {
        assert_eq!(
            check_command_safe(&v(&["sed", "-n", "1,5p", "file.txt"])),
            Some(true)
        );
        assert_eq!(
            check_command_safe(&v(&["sed", "-i", "s/foo/bar/g", "file.txt"])),
            Some(false)
        );
        assert_eq!(check_command_safe(&v(&["sed", "s/foo/bar/"])), Some(false));
    }

    #[test]
    fn test_sed_with_e_flag() {
        // sed -n -e 1p is valid and safe
        assert_eq!(
            check_command_safe(&v(&["sed", "-n", "-e", "1p", "file.txt"])),
            Some(true)
        );
        assert_eq!(
            check_command_safe(&v(&["sed", "-n", "-e", "1,5p", "file.txt"])),
            Some(true)
        );
        // sed -e without -n is not safe
        assert_eq!(
            check_command_safe(&v(&["sed", "-e", "s/foo/bar/"])),
            Some(false)
        );
    }

    #[test]
    fn test_sed_in_place_is_unsafe() {
        // sed -n with -i (in-place) is destructive
        assert_eq!(
            check_command_safe(&v(&["sed", "-n", "1p", "-i", "file.txt"])),
            Some(false)
        );
        assert_eq!(
            check_command_safe(&v(&["sed", "-n", "-i", "1p", "file.txt"])),
            Some(false)
        );
        assert_eq!(
            check_command_safe(&v(&["sed", "-n", "--in-place", "1p", "file.txt"])),
            Some(false)
        );
        assert_eq!(
            check_command_safe(&v(&["sed", "-i", "-n", "1p", "file.txt"])),
            Some(false)
        );
    }

    #[test]
    fn test_git_branch_read_only_flags() {
        // Already covered
        assert_eq!(
            check_command_safe(&v(&["git", "branch", "--show-current"])),
            Some(true)
        );
        assert_eq!(
            check_command_safe(&v(&["git", "branch", "new-branch"])),
            Some(false)
        );
        // Newly added read-only flags
        assert_eq!(
            check_command_safe(&v(&["git", "branch", "--merged", "main"])),
            Some(true)
        );
        assert_eq!(
            check_command_safe(&v(&["git", "branch", "--no-merged"])),
            Some(true)
        );
        assert_eq!(
            check_command_safe(&v(&["git", "branch", "--contains", "abc123"])),
            Some(true)
        );
        assert_eq!(
            check_command_safe(&v(&["git", "branch", "--sort=-committerdate"])),
            Some(true)
        );
        assert_eq!(
            check_command_safe(&v(&["git", "branch", "--column"])),
            Some(true)
        );
        // Inline value syntax (= form)
        assert_eq!(
            check_command_safe(&v(&["git", "branch", "--merged=main"])),
            Some(true)
        );
        assert_eq!(
            check_command_safe(&v(&["git", "branch", "--no-merged=main"])),
            Some(true)
        );
        assert_eq!(
            check_command_safe(&v(&["git", "branch", "--contains=abc123"])),
            Some(true)
        );
        assert_eq!(
            check_command_safe(&v(&["git", "branch", "--no-contains=abc123"])),
            Some(true)
        );
        assert_eq!(
            check_command_safe(&v(&["git", "branch", "--sort=-committerdate"])),
            Some(true)
        );
    }

    #[test]
    fn test_base64_safety() {
        assert_eq!(check_command_safe(&v(&["base64", "file.txt"])), Some(true));
        assert_eq!(
            check_command_safe(&v(&["base64", "--output", "out", "in"])),
            Some(false)
        );
        assert_eq!(
            check_command_safe(&v(&["base64", "-oout", "in"])),
            Some(false)
        );
    }

    #[test]
    fn test_sudo_recursion() {
        assert_eq!(check_command_safe(&v(&["sudo", "ls"])), Some(true));
        assert_eq!(
            check_command_safe(&v(&["sudo", "find", ".", "-delete"])),
            Some(false)
        );
    }

    #[test]
    fn test_dangerous_commands() {
        assert_eq!(check_command_dangerous(&v(&["rm", "-rf", "/"])), Some(true));
        assert_eq!(
            check_command_dangerous(&v(&["rm", "-f", "file"])),
            Some(true)
        );
        assert_eq!(check_command_dangerous(&v(&["rm", "file"])), Some(false));
        assert_eq!(
            check_command_dangerous(&v(&["dd", "if=/dev/zero", "of=/dev/sda"])),
            Some(true)
        );
        // Long forms and -R
        assert_eq!(
            check_command_dangerous(&v(&["rm", "--force", "--recursive", "/"])),
            Some(true)
        );
        assert_eq!(check_command_dangerous(&v(&["rm", "-R", "/"])), Some(true));
        // Lowercase -r
        assert_eq!(check_command_dangerous(&v(&["rm", "-r", "/"])), Some(true));
        // Combined short flags
        assert_eq!(check_command_dangerous(&v(&["rm", "-Rf", "/"])), Some(true));
        assert_eq!(check_command_dangerous(&v(&["rm", "-fR", "/"])), Some(true));
    }

    #[test]
    fn test_sudo_dangerous_recursion() {
        assert_eq!(
            check_command_dangerous(&v(&["sudo", "rm", "-rf", "/"])),
            Some(true)
        );
        assert_eq!(check_command_dangerous(&v(&["sudo", "ls"])), None);
    }

    #[test]
    fn test_xargs_safety() {
        assert_eq!(check_command_safe(&v(&["xargs", "-n1"])), Some(true));
        assert_eq!(check_command_safe(&v(&["xargs", "echo"])), Some(false));
        assert_eq!(
            check_command_safe(&v(&["xargs", "-n1", "echo"])),
            Some(true)
        );
        assert_eq!(
            check_command_safe(&v(&["xargs", "-n1", "rm", "-rf", "/"])),
            Some(false)
        );
        assert_eq!(check_command_safe(&v(&["xargs"])), Some(false));
    }
}
