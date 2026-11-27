# Per-terminal Bash history for Herdr. Source from interactive startup files.
# HERDR_SHELL_HISTORY_VERSION=1

# Do not opt shells into history if the user has disabled it.
[[ -n ${BASH_VERSION:-} && $- == *i* && ${HERDR_ENV:-} == 1 ]] || return 0
[[ -o history && -n ${HISTFILE:-} ]] || return 0
[[ ! -e $HISTFILE || -f $HISTFILE ]] || return 0

__herdr_history_flush() {
  local status=$? tmp
  if [[ -o history && ${HISTFILE:-} == "${_HERDR_HISTORY_FILE:-}" && -n ${HISTFILE:-} ]]; then
    if [[ ${_HERDR_HISTORY_NEEDS_LOAD:-} == 1 ]]; then
      # Bash loads HISTFILE after startup scripts. Wait until the first prompt
      # to remove any global history a user's rc loaded and load this pane once.
      builtin history -c
      builtin history -r "$HISTFILE" || :
      unset _HERDR_HISTORY_NEEDS_LOAD
    fi
    if (( BASH_VERSINFO[0] < 4 )); then
      # Bash 3.2 cannot append when all entries belong to this shell (including
      # an initially empty history). The file is dedicated to this terminal,
      # so persist its current list instead. Never rewrite the global history.
      # Bash 3.2 truncates -w destinations directly. Replace atomically so an
      # interrupted prompt flush leaves the previous saved list intact.
      if tmp=$(umask 077; command mktemp "${HISTFILE}.tmp.XXXXXXXX"); then
        if builtin history -w "$tmp" && command mv -f "$tmp" "$HISTFILE"; then
          # Drain the already-saved append queue when legacy Bash permits it.
          # This avoids duplicate appends on normal exit for seeded histories.
          builtin history -a /dev/null || :
        fi
        command rm -f "$tmp" || :
      fi
    else
      builtin history -a || :
    fi
  fi
  return "$status"
}

__herdr_history_setup() {
  local id=${HERDR_SHELL_HISTORY_ID:-} root file tmp declaration hook
  # New servers persist this random identity across moves, handoffs and restore.
  # Old servers do not have one; never guess from reusable pane numbers.
  [[ ${#id} == 32 && $id != *[!0-9a-fA-F]* ]] || return 0
  for declaration in "$(declare -p HISTFILE 2>/dev/null)" "$(declare -p PROMPT_COMMAND 2>/dev/null)"; do
    [[ $declaration =~ ^declare\ -[^[:space:]]*r ]] && return 0
  done
  if [[ ${_HERDR_HISTORY_PID:-} != "$$" || ${_HERDR_HISTORY_ID:-} != "$id" ]]; then
    root=${XDG_STATE_HOME:-$HOME/.local/state}
    [[ $root == /* ]] || root=$HOME/.local/state
    root=$root/herdr/shell-history/bash
    file=$root/$id.history
    # Keep history out of dotfiles/config repositories and preserve the shell umask.
    if ! (umask 077; command mkdir -p "$root"); then
      return 0
    fi
    if [[ ! -e $file && ! -L $file ]]; then
      tmp=$(umask 077; command mktemp "$root/.seed.XXXXXXXX") || return 0
      # Publish once without overwriting a concurrent shell's history file.
      command ln "$tmp" "$file" 2>/dev/null || :
      command rm -f "$tmp" || :
    fi
    [[ -f $file && ! -L $file && -O $file && -w $file ]] || return 0
    HISTFILE=$file
    _HERDR_HISTORY_FILE=$file
    _HERDR_HISTORY_ID=$id
    _HERDR_HISTORY_PID=$$
    _HERDR_HISTORY_NEEDS_LOAD=1
    if (( BASH_VERSINFO[0] < 4 )); then
      # Let Bash rewrite a stifled list at exit rather than force an append.
      shopt -u histappend
    else
      shopt -s histappend
    fi
  fi
  declaration=$(declare -p PROMPT_COMMAND 2>/dev/null) || declaration=
  if [[ $declaration == 'declare -a'* ]] &&
     (( BASH_VERSINFO[0] > 5 || (BASH_VERSINFO[0] == 5 && BASH_VERSINFO[1] >= 1) )); then
    for hook in "${PROMPT_COMMAND[@]}"; do
      [[ $hook == __herdr_history_flush ]] && return 0
    done
    PROMPT_COMMAND=(__herdr_history_flush "${PROMPT_COMMAND[@]}")
  else
    # Newline preserves existing comments and command separators. Older Bash
    # executes only element zero if PROMPT_COMMAND was declared as an array.
    case $'\n'${PROMPT_COMMAND:-}$'\n' in
      *$'\n__herdr_history_flush\n'*) ;;
      *) PROMPT_COMMAND="__herdr_history_flush${PROMPT_COMMAND:+$'\n'$PROMPT_COMMAND}" ;;
    esac
  fi
}

__herdr_history_setup
unset -f __herdr_history_setup
