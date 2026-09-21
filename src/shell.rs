use clap::ValueEnum;

#[derive(Clone, Copy, ValueEnum)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
}

/// A process cannot change its parent's directory, so a shell function
/// wraps `wt` and changes into the path that `wt add` prints on stdout.
/// When stdout is not a terminal, as in `cd "$(wt add 42)"`, the function
/// prints the path unchanged so scripts keep working.
pub fn init(shell: Shell) -> &'static str {
    match shell {
        Shell::Bash | Shell::Zsh => POSIX,
        Shell::Fish => FISH,
    }
}

// The `function` keyword keeps an existing `wt` alias from being expanded
// while the function is defined. Works with Bash 3.2, which macOS ships.
const POSIX: &str = r#"function wt {
  local arg subcommand= target
  for arg in "$@"; do
    case "$arg" in
      -*) ;;
      *) subcommand=$arg; break ;;
    esac
  done
  if [ "$subcommand" != add ] || [ ! -t 1 ]; then
    command wt "$@"
    return
  fi
  target="$(command wt "$@")" || return
  if [ -d "$target" ]; then
    cd -- "$target"
  elif [ -n "$target" ]; then
    printf '%s\n' "$target"
  fi
}
"#;

const FISH: &str = r#"function wt --description 'wt, changing into the directory printed by wt add'
    set -l subcommand
    for arg in $argv
        if not string match -q -- '-*' $arg
            set subcommand $arg
            break
        end
    end
    if test "$subcommand" != add; or not isatty stdout
        command wt $argv
        return
    end
    set -l target (command wt $argv)
    or return
    if test (count $target) -eq 1; and test -d "$target"
        cd $target
    else if test (count $target) -gt 0
        printf '%s\n' $target
    end
end
"#;
