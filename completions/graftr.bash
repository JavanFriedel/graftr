# Bash completion for graftr.
_graftr_complete() {
  local candidate
  COMPREPLY=()
  while IFS= read -r candidate; do
    COMPREPLY+=("$candidate")
  done < <(command "${COMP_WORDS[0]}" _complete -- "${COMP_WORDS[@]:1:COMP_CWORD}" 2>/dev/null)
  compopt -o filenames 2>/dev/null || :
}
complete -F _graftr_complete graftr
