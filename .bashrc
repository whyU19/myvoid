export XDG_CONFIG_HOME="$HOME/.config"
export XDG_DATA_HOME="$HOME/.local/share"
export XDG_CACHE_HOME="$HOME/.cache"
export XDG_STATE_HOME="$HOME/.local/state"
export PATH="$HOME/.local/bin:$HOME/.config/view:$HOME/.config/play:$HOME/.cargo/bin:$HOME/go/bin:$PATH"
export LESS="-R"
export LESS_TERMCAP_mb=$'\e[1;31m'
export LESS_TERMCAP_md=$'\e[1;36m'
export LESS_TERMCAP_me=$'\e[0m'
export LESS_TERMCAP_so=$'\e[1;44;33m'
export LESS_TERMCAP_se=$'\e[0m'
export LESS_TERMCAP_us=$'\e[1;32m'
export LESS_TERMCAP_ue=$'\e[0m'

HISTFILE="$XDG_STATE_HOME/bash/history"
HISTSIZE=50000
HISTFILESIZE=50000
HISTCONTROL=ignoreboth:erasedups
HISTIGNORE="ls:ll:la:cd:pwd:exit:clear:history"
HISTTIMEFORMAT="%F %T "

shopt -s histappend
shopt -s cmdhist
shopt -s lithist
shopt -s checkwinsize
shopt -s globstar
shopt -s nocaseglob
shopt -s dirspell
shopt -s autocd 2>/dev/null

PROMPT_COMMAND="history -a; history -n; ${PROMPT_COMMAND}"

parse_git_branch() {
  git branch 2>/dev/null | sed -n '/^\*/s/^\* //p'
}

RED='\[\033[38;2;243;139;168m\]'
GREEN='\[\033[38;2;166;227;161m\]'
YELLOW='\[\033[38;2;249;226;175m\]'
BLUE='\[\033[38;2;137;180;250m\]'
MAUVE='\[\033[38;2;203;166;247m\]'
RESET='\[\033[0m\]'

PS1="${GREEN}\u${RESET}@${BLUE}\h${RESET} ${YELLOW}\w${RESET} \$(parse_git_branch && echo \"(${MAUVE}\$(parse_git_branch)${RESET})\")\n${BLUE}>${RESET} "

export FZF_DEFAULT_OPTS="
  --height=60%
  --layout=reverse
  --border=none
  --no-separator
  --no-scrollbar
  --prompt='  '
  --pointer='  '
  --marker='  '
  --preview='_fzf_preview {}'
  --preview-window=right,65%,wrap,border-none
  --color=bg:#1e1e2e,bg+:#89b4fa,spinner:#89b4fa,hl:#94e2d5
  --color=fg:#cdd6f4,header:#89b4fa,info:#a6e3a1,pointer:#cba6f7
  --color=marker:#cba6f7,fg+:#1e1e2e,prompt:#89b4fa,hl+:#f9e2af,gutter:#1e1e2e
"

alias xi='sudo xbps-install -S'
alias xr='sudo xbps-remove -R'
alias xq='xbps-query -Rs'
alias xu='sudo xbps-install -Su'
alias xc='sudo xbps-remove -Oo'
alias xl='xbps-query -l'

export BAT_THEME=ansi
alias ls='eza --group-directories-first'
alias ll='eza -l --group-directories-first --git --header'
alias la='eza -la --group-directories-first --git --header'
alias lt='eza --tree --level=2 --group-directories-first'
alias cat='bat --paging=never'
alias du='dust'
alias df='duf'

if [ -f /usr/share/bash-completion/bash_completion ]; then
  . /usr/share/bash-completion/bash_completion
fi

[ -f ~/.bashrc.local ] && source ~/.bashrc.local
