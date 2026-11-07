# SpritX Pastel prompt, composed with existing precmd hooks.
autoload -Uz add-zsh-hook
__spritx_node_dir=''
__spritx_node_version=''
__spritx_command_started() {
    local line=${1//[$'\001'-$'\037'$'\177']/ }
    print -rn -- $'\e]133;E;'$line$'\a\e]133;C\a'
}
add-zsh-hook preexec __spritx_command_started
# Percent-encode bytes < 0x20, 0x7F, and % # ?, so a crafted directory name
# cannot break the OSC 7 payload or inject escape sequences.
__spritx_osc7_escape() {
    local value=${1-} LC_ALL=C out='' byte i
    for (( i = 0; i < ${#value}; i++ )); do
        byte=${value:i:1}
        case "$byte" in
        [![:print:]]|%|"#"|"?")
            out+=$(printf '%%%02X' "'$byte") ;;
        *)
            out+=$byte ;;
        esac
    done
    printf '%s' "$out"
}
__spritx_prompt() {
    local result=$? folder=${PWD:t} arrow=166\;227\;161
    local spritx_cwd
    spritx_cwd=$(__spritx_osc7_escape "${PWD-}")
    printf '\e]7;file://localhost%s\a' "$spritx_cwd"
    printf '\e]133;D;%s\a' "$result"
    [[ "$PWD" == "$HOME" ]] && folder='~'
    [[ -z "$folder" ]] && folder=/
    if [[ "$__spritx_node_dir" != "$PWD" ]]; then
        __spritx_node_dir=$PWD; __spritx_node_version=''
        (( $+commands[node] )) && __spritx_node_version=$(node --version 2>/dev/null)
    fi
    # % expansion is still needed for colors; escape literal user/path percent.
    local user_name=${USER//\%/%%}
    folder=${folder//\%/%%}
    unsetopt prompt_subst
    print -rn -- $'\e]7;file://localhost'$spritx_cwd$'\a'
    PROMPT='%F{#45475a}%K{#45475a}%F{#cdd6f4} '$user_name' '
    PROMPT+='%F{#45475a}%K{#fab387}%F{#1e1e2e} '$folder' '
    if [[ -n "$__spritx_node_version" ]]; then
        PROMPT+='%F{#fab387}%K{#a6e3a1}%F{#a6e3a1}%K{#94e2d5}%F{#1e1e2e}  '$__spritx_node_version' '
        PROMPT+='%F{#94e2d5}%K{#89b4fa}%F{#89b4fa}%K{#f5c2e7}'
    else
        PROMPT+='%F{#fab387}%K{#f5c2e7}'
    fi
    PROMPT+='%F{#1e1e2e}  %D{%H:%M} %k%F{#f5c2e7}%f'$'\n'
    (( result != 0 )) && arrow=243\;139\;168
    PROMPT+=$'%{\e[38;2;'$arrow$'m%}❯%{\e[0m%} '
    # Literal path/user text must never be evaluated as shell code.
    unsetopt prompt_subst
}
add-zsh-hook precmd __spritx_prompt
