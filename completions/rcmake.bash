# SPDX-License-Identifier: MIT OR Apache-2.0
# Bash completion for rcmake (compatible with Bash 3.2+ without bash-completion).

_rcmake_compgen_dirs() {
    COMPREPLY=( $(compgen -d -- "$1") )
}

_rcmake_compgen_files() {
    COMPREPLY=( $(compgen -f -- "$1") )
}

_rcmake_compgen_eq_dirs() {
    local prefix="$1"
    local val="$2"
    local d
    COMPREPLY=()
    while IFS= read -r d; do
        COMPREPLY+=( "${prefix}${d}/" )
    done < <(compgen -d -- "$val")
    if type compopt >/dev/null 2>&1; then
        compopt -o nospace 2>/dev/null
    fi
}

_rcmake_compgen_eq_files() {
    local prefix="$1"
    local val="$2"
    local f
    COMPREPLY=()
    while IFS= read -r f; do
        if [[ -d "$f" ]]; then
            COMPREPLY+=( "${prefix}${f}/" )
        else
            COMPREPLY+=( "${prefix}${f}" )
        fi
    done < <(compgen -f -- "$val")
    if type compopt >/dev/null 2>&1; then
        compopt -o nospace 2>/dev/null
    fi
}

_rcmake_completions() {
    local cur prev pprev first
    COMPREPLY=()
    cur="${COMP_WORDS[COMP_CWORD]}"
    prev=""
    pprev=""
    if [[ $COMP_CWORD -gt 0 ]]; then
        prev="${COMP_WORDS[COMP_CWORD-1]}"
    fi
    if [[ $COMP_CWORD -gt 1 ]]; then
        pprev="${COMP_WORDS[COMP_CWORD-2]}"
    fi
    first="${COMP_WORDS[1]:-}"

    # Handle default Bash COMP_WORDBREAKS splitting of "--flag=value" into "--flag", "=", "value".
    if [[ "$prev" == "=" && -n "$pprev" ]]; then
        case "$pprev" in
            --diagnostics)
                COMPREPLY=( $(compgen -W "cmake rich" -- "$cur") )
                return 0
                ;;
            --fetch-lock)
                COMPREPLY=( $(compgen -W "off auto update frozen" -- "$cur") )
                return 0
                ;;
            --hermetic)
                COMPREPLY=( $(compgen -W "warn strict" -- "$cur") )
                return 0
                ;;
            --profile|--install-prefix|--prefix|-B)
                _rcmake_compgen_dirs "$cur"
                return 0
                ;;
            --color)
                COMPREPLY=( $(compgen -W "auto always never" -- "$cur") )
                return 0
                ;;
            --config)
                COMPREPLY=( $(compgen -W "Debug Release RelWithDebInfo MinSizeRel" -- "$cur") )
                return 0
                ;;
            --log-level|--loglevel)
                COMPREPLY=( $(compgen -W "ERROR WARNING NOTICE STATUS VERBOSE DEBUG TRACE" -- "$cur") )
                return 0
                ;;
            --trace-format)
                COMPREPLY=( $(compgen -W "human json-v1" -- "$cur") )
                return 0
                ;;
            --profiling-format)
                COMPREPLY=( $(compgen -W "google-trace" -- "$cur") )
                return 0
                ;;
            --resolve-package-references)
                COMPREPLY=( $(compgen -W "on only off" -- "$cur") )
                return 0
                ;;
            --version)
                COMPREPLY=( $(compgen -W "json-v1" -- "$cur") )
                return 0
                ;;
            --list-presets)
                if [[ "$first" == "--build" || "$first" == "--workflow" ]]; then
                    COMPREPLY=( $(compgen -W "defined" -- "$cur") )
                else
                    COMPREPLY=( $(compgen -W "configure build test package workflow all defined configure-defined build-defined test-defined package-defined workflow-defined all-defined" -- "$cur") )
                fi
                return 0
                ;;
            --toolchain|--presets-file|--project-file|--trace-source|--trace-redirect|--profiling-output|--graphviz|--sarif-output)
                _rcmake_compgen_files "$cur"
                return 0
                ;;
        esac
    fi

    # Subcommand: rcmake explain
    if [[ "$first" == "explain" && $COMP_CWORD -ge 2 ]]; then
        case "$cur" in
            --color=*)
                COMPREPLY=( $(compgen -W "auto always never" -P "--color=" -- "${cur#--color=}") )
                return 0
                ;;
            --config=*)
                COMPREPLY=( $(compgen -W "Debug Release RelWithDebInfo MinSizeRel" -P "--config=" -- "${cur#--config=}") )
                return 0
                ;;
            -B=*)
                _rcmake_compgen_eq_dirs "-B=" "${cur#-B=}"
                return 0
                ;;
        esac
        case "$prev" in
            -B)
                _rcmake_compgen_dirs "$cur"
                return 0
                ;;
            --color)
                COMPREPLY=( $(compgen -W "auto always never" -- "$cur") )
                return 0
                ;;
            --config)
                COMPREPLY=( $(compgen -W "Debug Release RelWithDebInfo MinSizeRel" -- "$cur") )
                return 0
                ;;
            --filter)
                return 0
                ;;
        esac
        if [[ "$cur" == -* ]]; then
            COMPREPLY=( $(compgen -W "-B --target --var --config --filter --json --verbose --color=auto --color=always --color=never -h --help" -- "$cur") )
        fi
        return 0
    fi

    # Mode: rcmake --build
    if [[ "$first" == "--build" && $COMP_CWORD -ge 2 ]]; then
        case "$cur" in
            --resolve-package-references=*)
                COMPREPLY=( $(compgen -W "on only off" -P "--resolve-package-references=" -- "${cur#--resolve-package-references=}") )
                return 0
                ;;
            --list-presets=*)
                COMPREPLY=( $(compgen -W "defined" -P "--list-presets=" -- "${cur#--list-presets=}") )
                return 0
                ;;
            --presets-file=*)
                _rcmake_compgen_eq_files "--presets-file=" "${cur#--presets-file=}"
                return 0
                ;;
            --config=*)
                COMPREPLY=( $(compgen -W "Debug Release RelWithDebInfo MinSizeRel" -P "--config=" -- "${cur#--config=}") )
                return 0
                ;;
        esac
        case "$prev" in
            --build)
                if [[ "$cur" == -* ]]; then
                    COMPREPLY=( $(compgen -W "--preset --presets-file --list-presets" -- "$cur") )
                else
                    _rcmake_compgen_dirs "$cur"
                fi
                return 0
                ;;
            --presets-file)
                _rcmake_compgen_files "$cur"
                return 0
                ;;
            --config)
                COMPREPLY=( $(compgen -W "Debug Release RelWithDebInfo MinSizeRel" -- "$cur") )
                return 0
                ;;
            --resolve-package-references)
                COMPREPLY=( $(compgen -W "on only off" -- "$cur") )
                return 0
                ;;
            --preset|-t|--target|-j|--parallel)
                return 0
                ;;
        esac
        if [[ "$cur" == -* ]]; then
            COMPREPLY=( $(compgen -W "--preset --presets-file --list-presets --list-presets=defined -j --parallel -t --target --config --clean-first --resolve-package-references=on --resolve-package-references=only --resolve-package-references=off -v --verbose" -- "$cur") )
        else
            _rcmake_compgen_dirs "$cur"
        fi
        return 0
    fi

    # Mode: rcmake --install
    if [[ "$first" == "--install" && $COMP_CWORD -ge 2 ]]; then
        case "$cur" in
            --prefix=*)
                _rcmake_compgen_eq_dirs "--prefix=" "${cur#--prefix=}"
                return 0
                ;;
            --config=*)
                COMPREPLY=( $(compgen -W "Debug Release RelWithDebInfo MinSizeRel" -P "--config=" -- "${cur#--config=}") )
                return 0
                ;;
        esac
        case "$prev" in
            --install|--prefix)
                _rcmake_compgen_dirs "$cur"
                return 0
                ;;
            --config)
                COMPREPLY=( $(compgen -W "Debug Release RelWithDebInfo MinSizeRel" -- "$cur") )
                return 0
                ;;
            --component|--default-directory-permissions|-j|--parallel)
                return 0
                ;;
        esac
        if [[ "$cur" == -* ]]; then
            COMPREPLY=( $(compgen -W "--config --component --default-directory-permissions -j --parallel --prefix --strip -v --verbose" -- "$cur") )
        else
            _rcmake_compgen_dirs "$cur"
        fi
        return 0
    fi

    # Mode: rcmake --workflow
    if [[ "$first" == "--workflow" && $COMP_CWORD -ge 2 ]]; then
        case "$cur" in
            --list-presets=*)
                COMPREPLY=( $(compgen -W "defined" -P "--list-presets=" -- "${cur#--list-presets=}") )
                return 0
                ;;
            --presets-file=*)
                _rcmake_compgen_eq_files "--presets-file=" "${cur#--presets-file=}"
                return 0
                ;;
        esac
        case "$prev" in
            --presets-file)
                _rcmake_compgen_files "$cur"
                return 0
                ;;
            --preset)
                return 0
                ;;
        esac
        COMPREPLY=( $(compgen -W "--preset --presets-file --list-presets --list-presets=defined --fresh" -- "$cur") )
        return 0
    fi

    # Mode: rcmake -E
    if [[ "$first" == "-E" && $COMP_CWORD -eq 2 ]]; then
        local e_cmds="bin2c capabilities cat chdir compare_files copy copy_directory copy_directory_if_different copy_directory_if_newer copy_if_different copy_if_newer create_hardlink create_symlink echo echo_append env environment false make_directory md5sum remove remove_directory rename rm sha1sum sha224sum sha256sum sha384sum sha512sum sleep tar time touch touch_nocreate true"
        COMPREPLY=( $(compgen -W "$e_cmds" -- "$cur") )
        return 0
    fi

    # Handle --flag=value options in configure mode
    case "$cur" in
        --diagnostics=*)
            COMPREPLY=( $(compgen -W "cmake rich" -P "--diagnostics=" -- "${cur#--diagnostics=}") )
            return 0
            ;;
        --fetch-lock=*)
            COMPREPLY=( $(compgen -W "off auto update frozen" -P "--fetch-lock=" -- "${cur#--fetch-lock=}") )
            return 0
            ;;
        --hermetic=*)
            COMPREPLY=( $(compgen -W "warn strict" -P "--hermetic=" -- "${cur#--hermetic=}") )
            return 0
            ;;
        --profile=*)
            _rcmake_compgen_eq_dirs "--profile=" "${cur#--profile=}"
            return 0
            ;;
        --log-level=*)
            COMPREPLY=( $(compgen -W "ERROR WARNING NOTICE STATUS VERBOSE DEBUG TRACE" -P "--log-level=" -- "${cur#--log-level=}") )
            return 0
            ;;
        --trace-format=*)
            COMPREPLY=( $(compgen -W "human json-v1" -P "--trace-format=" -- "${cur#--trace-format=}") )
            return 0
            ;;
        --profiling-format=*)
            COMPREPLY=( $(compgen -W "google-trace" -P "--profiling-format=" -- "${cur#--profiling-format=}") )
            return 0
            ;;
        --list-presets=*)
            COMPREPLY=( $(compgen -W "configure build test package workflow all defined configure-defined build-defined test-defined package-defined workflow-defined all-defined" -P "--list-presets=" -- "${cur#--list-presets=}") )
            return 0
            ;;
        --version=*)
            COMPREPLY=( $(compgen -W "json-v1" -P "--version=" -- "${cur#--version=}") )
            return 0
            ;;
        --install-prefix=*)
            _rcmake_compgen_eq_dirs "--install-prefix=" "${cur#--install-prefix=}"
            return 0
            ;;
        --toolchain=*|--presets-file=*|--project-file=*|--trace-source=*|--trace-redirect=*|--profiling-output=*|--graphviz=*|--sarif-output=*)
            local opt_prefix="${cur%%=*}="
            _rcmake_compgen_eq_files "$opt_prefix" "${cur#*=}"
            return 0
            ;;
    esac

    # Handle previous flag expecting a value
    case "$prev" in
        -S|-B|--install-prefix)
            _rcmake_compgen_dirs "$cur"
            return 0
            ;;
        -C|-P|--toolchain|--presets-file|--project-file|--trace-source|--trace-redirect|--profiling-output|--graphviz|--sarif-output)
            _rcmake_compgen_files "$cur"
            return 0
            ;;
        -G)
            local IFS=$'\n'
            COMPREPLY=( $(compgen -W $'Ninja\nNinja Multi-Config\nUnix Makefiles\nXcode\nVisual Studio 17 2022\nVisual Studio 18 2026' -- "$cur") )
            return 0
            ;;
        --log-level|--loglevel)
            COMPREPLY=( $(compgen -W "ERROR WARNING NOTICE STATUS VERBOSE DEBUG TRACE" -- "$cur") )
            return 0
            ;;
        --trace-format)
            COMPREPLY=( $(compgen -W "human json-v1" -- "$cur") )
            return 0
            ;;
        --profiling-format)
            COMPREPLY=( $(compgen -W "google-trace" -- "$cur") )
            return 0
            ;;
        --list-presets)
            COMPREPLY=( $(compgen -W "configure build test package workflow all defined configure-defined build-defined test-defined package-defined workflow-defined all-defined" -- "$cur") )
            return 0
            ;;
        -D|-U|-T|-A|--preset|--debug-find-pkg|--debug-find-var)
            return 0
            ;;
    esac

    local native_opts="--diagnostics=cmake --diagnostics=rich --profile --no-probe-cache --fetch-lock=off --fetch-lock=auto --fetch-lock=update --fetch-lock=frozen --hermetic --hermetic=warn --hermetic=strict"
    local cmake_opts="-S -B -C -D -U -G -T -A --toolchain --install-prefix --project-file --preset --presets-file --list-presets --fresh --build --install --workflow -E -P -N -L -LA -LH -LAH -LR -LRA -LRH -LRAH --find-package --graphviz= --sarif-output= --system-information --print-config-dir --log-level= --log-context --debug-trycompile --debug-output --debug-find --debug-find-pkg= --debug-find-var= --trace --trace-expand --trace-format= --trace-source= --trace-redirect= --check-system-vars --warn-uninitialized --no-warn-unused-cli --compile-no-warning-as-error --link-no-warning-as-error --profiling-format= --profiling-output= -Wdev -Wno-dev -Werror=dev -Wno-error=dev -Wdeprecated -Wno-deprecated -Werror=deprecated -Wno-error=deprecated -Wauthor -Wno-author -Wpolicy -Wno-policy -Wstrict -Wno-strict -Wexperimental -Wno-experimental -h -H --help --version --version=json-v1 --help-full --help-manual --help-manual-list --help-command --help-command-list --help-commands --help-diagnostic --help-diagnostic-list --help-diagnostics --help-module --help-module-list --help-modules --help-policy --help-policy-list --help-policies --help-property --help-property-list --help-properties --help-variable --help-variable-list --help-variables"

    if [[ $COMP_CWORD -eq 1 ]]; then
        if [[ "$cur" == -* ]]; then
            COMPREPLY=( $(compgen -W "$native_opts $cmake_opts" -- "$cur") )
        else
            COMPREPLY=( $(compgen -W "explain" -- "$cur") )
            local d
            while IFS= read -r d; do
                COMPREPLY+=( "$d" )
            done < <(compgen -d -- "$cur")
        fi
        return 0
    fi

    if [[ "$cur" == -* ]]; then
        COMPREPLY=( $(compgen -W "$native_opts $cmake_opts" -- "$cur") )
    else
        _rcmake_compgen_dirs "$cur"
    fi
    return 0
}

complete -o default -F _rcmake_completions rcmake
