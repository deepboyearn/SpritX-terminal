# SpritX Pastel prompt. Source after the user's rc file.
__spritx_node_dir=''
__spritx_node_version=''
__spritx_command_started() {
    local line
    line=$(HISTTIMEFORMAT= builtin history 1)
    [[ "$line" =~ ^[[:space:]]*[0-9]+[[:space:]]+(.*)$ ]] && line=${BASH_REMATCH[1]}
    line=${line//[$'\001'-$'\037'$'\177']/ }
    printf '\e]133;E;%s\a\e]133;C\a' "$line"
}
PS0=${PS0-}'$(__spritx_command_started)'
__spritx_command_finished() {
    local status=$?
    printf '\e]133;D;%s\a' "$status"
    return "$status"
}
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
    local status=$? folder user_name clock arrow=166\;227\;161
    local spritx_cwd
    spritx_cwd=$(__spritx_osc7_escape "${PWD-}")
    printf '\e]7;file://localhost%s\a' "$spritx_cwd"
    user_name=${USER:-$(id -un)}
    folder=${PWD##*/}; [[ "$PWD" == "$HOME" ]] && folder='~'
    [[ -z "$folder" ]] && folder=/
    user_name=${user_name//[$'\001'-$'\037'$'\177']/}
    folder=${folder//[$'\001'-$'\037'$'\177']/}
    if [[ "$__spritx_node_dir" != "$PWD" ]]; then
        __spritx_node_dir=$PWD; __spritx_node_version=''
        if command -v node >/dev/null 2>&1; then
            __spritx_node_version=$(node --version 2>/dev/null)
            [[ "$__spritx_node_version" =~ ^v[0-9]+\.[0-9]+\.[0-9]+ ]] || __spritx_node_version=''
        fi
    fi
    printf -v clock '%(%H:%M)T' -1
    # Expand only our parameter references. Bash does not recursively evaluate
    # $(...) or backticks inside a parameter's value; literal path text must
    # never be concatenated into the prompt expression itself.
    __spritx_prompt_cwd=$spritx_cwd
    __spritx_prompt_user=$user_name
    __spritx_prompt_folder=$folder
    shopt -s promptvars
    PS1='\[\e]7;file://localhost${__spritx_prompt_cwd}\a\]'
    PS1+='\[\e[38;2;69;71;90m\]\[\e[48;2;69;71;90;38;2;205;214;244m\] ${__spritx_prompt_user} '
    PS1+='\[\e[38;2;69;71;90;48;2;250;179;135m\]\[\e[38;2;30;30;46m\] ${__spritx_prompt_folder} '
    if [[ -n "$__spritx_node_version" ]]; then
        PS1+='\[\e[38;2;250;179;135;48;2;166;227;161m\]\[\e[38;2;166;227;161;48;2;148;226;213m\]'
        PS1+='\[\e[38;2;30;30;46m\]  ${__spritx_node_version} '
        PS1+='\[\e[38;2;148;226;213;48;2;137;180;250m\]\[\e[38;2;137;180;250;48;2;245;194;231m\]'
    else
        PS1+='\[\e[38;2;250;179;135;48;2;245;194;231m\]'
    fi
    PS1+='\[\e[38;2;30;30;46m\]  '$clock' \[\e[38;2;245;194;231;49m\]\[\e[0m\]\n'
    (( status != 0 )) && arrow=243\;139\;168
    PS1+='\[\e[38;2;'$arrow'm\]❯\[\e[0m\] '

}
# Preserve existing prompt hooks (including shell integration), then style ours.
if [[ $(declare -p PROMPT_COMMAND 2>/dev/null) == 'declare -a'* ]]; then
    PROMPT_COMMAND=(__spritx_command_finished "${PROMPT_COMMAND[@]}" __spritx_prompt)
else
    PROMPT_COMMAND="__spritx_command_finished; ${PROMPT_COMMAND:+$PROMPT_COMMAND; }__spritx_prompt"
fi
