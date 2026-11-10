# SimplePackagemAnagingSystem (SPAS) ebuild environment.
# Implements the package manager side of EAPI 7, 8 and 9 for `hoard`.
# hoard runs this file with bash; SPAS_MODE selects what happens:
#   depend  source the ebuild and write its metadata to SPAS_META_OUT
#   phase   run EBUILD_PHASE, starting from SPAS_ENV_IN (or a fresh ebuild
#           source) and saving the resulting environment to SPAS_ENV_OUT

shopt -s extglob
export SPAS_MAIN_PID=${BASHPID}
trap 'exit 1' USR1

# ------------------------------------------------------------------ output

__spas_log() {
	# level message...: remember elog/ewarn/eerror for the summary at the end
	[[ -n ${T} && -d ${T} && -w ${T} ]] || return 0
	printf '%s %s %s\n' "${EBUILD_PHASE:-unknown}" "$1" "${*:2}" >> "${T}/spas-elog" 2>/dev/null
}
__spas_c() { [[ -n ${NOCOLOR} || ! -t 2 ]] && return; printf '\e[%sm' "$1"; }
einfo()  { printf ' %s*%s %s\n' "$(__spas_c '32;01')" "$(__spas_c 0)" "$*" >&2; }
einfon() { printf ' %s*%s %s' "$(__spas_c '32;01')" "$(__spas_c 0)" "$*" >&2; }
elog()   { einfo "$@"; __spas_log LOG "$*"; }
ewarn()  { printf ' %s*%s %s\n' "$(__spas_c '33;01')" "$(__spas_c 0)" "$*" >&2; __spas_log WARN "$*"; }
eerror() { printf ' %s*%s %s\n' "$(__spas_c '31;01')" "$(__spas_c 0)" "$*" >&2; __spas_log ERROR "$*"; }
eqawarn() { printf ' %s*%s QA Notice: %s\n' "$(__spas_c '33;01')" "$(__spas_c 0)" "$*" >&2; __spas_log QA "$*"; }
ebegin() { einfo "$* ..."; }
eend() {
	local r=${1:-0}
	shift
	if [[ ${r} -ne 0 ]]; then
		eerror "${*:-failed}"
		printf '   [ %s!!%s ]\n' "$(__spas_c '31;01')" "$(__spas_c 0)" >&2
	else
		printf '   [ %sok%s ]\n' "$(__spas_c '32;01')" "$(__spas_c 0)" >&2
	fi
	return "${r}"
}
debug-print() { [[ -n ${ECLASS_DEBUG_OUTPUT} ]] && printf 'debug: %s\n' "$*" >&2; return 0; }
debug-print-function() { debug-print "${1}: entering function, parameters: ${*:2}"; }
debug-print-section() { debug-print "now in section ${*}"; }

# ------------------------------------------------------------------ errors

die() {
	if [[ $1 == -n ]]; then
		shift
		if [[ -n ${SPAS_NONFATAL} ]]; then
			[[ $# -gt 0 ]] && eerror "$*"
			return 1
		fi
	fi
	local i
	eerror "ERROR: ${CATEGORY}/${PF}::${PORTAGE_REPO_NAME} failed (${EBUILD_PHASE} phase):"
	eerror "  ${*:-(no error message)}"
	eerror
	eerror "Call stack:"
	for (( i = 1; i < ${#FUNCNAME[@]}; i++ )); do
		eerror "  ${BASH_SOURCE[i]##*/}, line ${BASH_LINENO[i-1]}:  called ${FUNCNAME[i-1]}"
	done
	[[ -n ${T} ]] && eerror "The build log is ${T}/build.log"
	if [[ ${BASHPID} != "${SPAS_MAIN_PID}" ]]; then
		kill -s USR1 "${SPAS_MAIN_PID}" 2>/dev/null
	fi
	exit 1
}

# helpers fail through this so `nonfatal` works
__helper_die() {
	if [[ -n ${SPAS_NONFATAL} ]]; then
		eerror "$*"
		return 1
	fi
	die "$@"
}

nonfatal() {
	[[ $# -gt 0 ]] || die "nonfatal: no command given"
	SPAS_NONFATAL=1 "$@"
}

if [[ ${EAPI} == [78] ]]; then
	assert() {
		local x pipestatus=${PIPESTATUS[*]}
		for x in ${pipestatus}; do
			[[ ${x} -eq 0 ]] || die "$@"
		done
	}
fi

pipestatus() {
	local status=( "${PIPESTATUS[@]}" ) s ret=0
	[[ $1 == -v ]] && { printf '%s\n' "${status[*]}"; shift; }
	[[ $# -eq 0 ]] || die "usage: pipestatus [-v]"
	for s in "${status[@]}"; do
		[[ ${s} -ne 0 ]] && ret=${s}
	done
	return "${ret}"
}

edo() {
	[[ $# -gt 0 ]] || die "edo: no command given"
	einfo "$(printf '%q ' "$@")"
	"$@" || die -n "Failed to run command: $*"
}

# ------------------------------------------------------------------ queries

has() {
	local needle=$1 x
	shift
	for x in "$@"; do
		[[ ${x} == "${needle}" ]] && return 0
	done
	return 1
}
if [[ ${EAPI} == 7 ]]; then
	hasv() { has "$@" && echo "$1"; }
	hasq() { has "$@"; }
	useq() { use "$@"; }
fi

__spas_build_use_set() {
	declare -gA __SPAS_USE=() __SPAS_IUSE_EFF=()
	local f
	for f in ${USE}; do __SPAS_USE[${f}]=1; done
	for f in ${SPAS_IUSE_EFFECTIVE}; do __SPAS_IUSE_EFF[${f}]=1; done
}

use() {
	local f=$1 neg=0
	[[ ${f} == !* ]] && { neg=1; f=${f#!}; }
	[[ $# -eq 1 && -n ${f} ]] || die "usage: use [!]<flag>"
	if [[ ${EBUILD_PHASE} == depend ]]; then
		die "use() called in global scope"
	fi
	if [[ -z ${__SPAS_IUSE_EFF[${f}]} ]]; then
		eqawarn "USE flag '${f}' is not in IUSE for ${CATEGORY}/${PF}"
	fi
	if [[ -n ${__SPAS_USE[${f}]} ]]; then
		return ${neg}
	fi
	return $(( ! neg ))
}

usev() {
	if use "$1"; then
		if [[ ${EAPI} != 7 && $# -ge 2 ]]; then echo "$2"; else echo "${1#!}"; fi
		return 0
	fi
	return 1
}

usex() {
	if use "$1"; then
		echo "${2-yes}$4"
	else
		echo "${3-no}$5"
	fi
	return 0
}

use_with() {
	[[ -n $1 ]] || die "use_with: missing flag"
	local opt=${2-$1}
	local val=${3+=$3}
	if use "$1"; then echo "--with-${opt}${val}"; else echo "--without-${opt}"; fi
	return 0
}

use_enable() {
	[[ -n $1 ]] || die "use_enable: missing flag"
	local opt=${2-$1}
	local val=${3+=$3}
	if use "$1"; then echo "--enable-${opt}${val}"; else echo "--disable-${opt}"; fi
	return 0
}

in_iuse() {
	[[ $# -eq 1 ]] || die "usage: in_iuse <flag>"
	[[ -n ${__SPAS_IUSE_EFF[$1]} ]]
}

get_libdir() {
	local abi=${ABI:-${DEFAULT_ABI:-default}}
	local v="LIBDIR_${abi}"
	echo "${!v:-lib}"
}

__spas_root_flag() {
	case $1 in
		-r|-b|-d) echo "$1" ;;
		*) echo "-r" ;;
	esac
}
has_version() {
	local r=-r
	if [[ $1 == -[rbd] ]]; then r=$1; shift; fi
	[[ $# -eq 1 ]] || die "usage: has_version [-b|-d|-r] <atom>"
	"${SPAS_BIN}" __internal has_version "${r}" "$1"
	local s=$?
	[[ ${s} -le 1 ]] || die "has_version failed"
	return ${s}
}
best_version() {
	local r=-r
	if [[ $1 == -[rbd] ]]; then r=$1; shift; fi
	[[ $# -eq 1 ]] || die "usage: best_version [-b|-d|-r] <atom>"
	"${SPAS_BIN}" __internal best_version "${r}" "$1"
	local s=$?
	[[ ${s} -le 1 ]] || die "best_version failed"
	return ${s}
}

# ------------------------------------------------------------------ versions

# split into [sep0 comp1 sep1 comp2 ... compN sepN] (PMS version components)
__spas_ver_split() {
	local v=$1 c
	__SPAS_VC=( "" )
	if [[ ${v} =~ ^([^a-zA-Z0-9]+) ]]; then
		__SPAS_VC[0]=${BASH_REMATCH[1]}
		v=${v:${#BASH_REMATCH[1]}}
	fi
	while [[ -n ${v} ]]; do
		if [[ ${v} =~ ^([0-9]+) || ${v} =~ ^([a-zA-Z]+) ]]; then
			c=${BASH_REMATCH[1]}
		else
			break
		fi
		__SPAS_VC+=( "${c}" )
		v=${v:${#c}}
		c=
		[[ ${v} =~ ^([^a-zA-Z0-9]+) ]] && c=${BASH_REMATCH[1]}
		__SPAS_VC+=( "${c}" )
		v=${v:${#c}}
	done
}

__spas_ver_range() {
	# range max -> sets __spas_rs __spas_re
	if [[ $1 == *-* ]]; then
		__spas_rs=${1%-*}
		__spas_re=${1#*-}
		[[ -z ${__spas_re} ]] && __spas_re=$2
	else
		__spas_rs=$1
		__spas_re=$1
	fi
	[[ ${__spas_rs} =~ ^[0-9]+$ && ${__spas_re} =~ ^[0-9]+$ ]] || die "invalid version range '$1'"
	(( __spas_re > $2 )) && __spas_re=$2
}

ver_cut() {
	[[ $# -eq 1 || $# -eq 2 ]] || die "usage: ver_cut <range> [<version>]"
	local __spas_rs __spas_re i out=
	__spas_ver_split "${2-${PV}}"
	local n=$(( (${#__SPAS_VC[@]} - 1) / 2 ))
	__spas_ver_range "$1" "${n}"
	(( __spas_rs < 1 )) && __spas_rs=1
	for (( i = __spas_rs; i <= __spas_re; i++ )); do
		(( i > __spas_rs )) && out+=${__SPAS_VC[2*i-2]}
		out+=${__SPAS_VC[2*i-1]}
	done
	echo "${out}"
}

ver_rs() {
	local v
	if (( $# % 2 == 1 )); then
		v=${!#}
		set -- "${@:1:$#-1}"
	else
		v=${PV}
	fi
	[[ $# -ge 2 ]] || die "usage: ver_rs <range> <repl> [<range> <repl>...] [<version>]"
	__spas_ver_split "${v}"
	local n=$(( (${#__SPAS_VC[@]} - 1) / 2 )) i __spas_rs __spas_re
	while [[ $# -ge 2 ]]; do
		__spas_ver_range "$1" "${n}"
		for (( i = __spas_rs; i <= __spas_re; i++ )); do
			if (( i > 0 && i < n )) || [[ -n ${__SPAS_VC[2*i]} ]]; then
				__SPAS_VC[2*i]=$2
			fi
		done
		shift 2
	done
	local IFS=
	echo "${__SPAS_VC[*]}"
}

ver_test() {
	local a op b
	if [[ $# -eq 2 ]]; then a=${PVR}; op=$1; b=$2; else a=$1; op=$2; b=$3; fi
	local r
	r=$("${SPAS_BIN}" __internal vercmp "${a}" "${b}") || die "ver_test: invalid version '${a}' or '${b}'"
	case ${op} in
		-eq) [[ ${r} -eq 0 ]] ;;
		-ne) [[ ${r} -ne 0 ]] ;;
		-lt) [[ ${r} -lt 0 ]] ;;
		-le) [[ ${r} -le 0 ]] ;;
		-gt) [[ ${r} -gt 0 ]] ;;
		-ge) [[ ${r} -ge 0 ]] ;;
		*) die "ver_test: bad operator ${op}" ;;
	esac
}

ver_replacing() {
	[[ $# -eq 2 ]] || die "usage: ver_replacing <op> <version>"
	local v
	for v in ${REPLACING_VERSIONS}; do
		ver_test "${v}" "$1" "$2" && return 0
	done
	return 1
}

# ------------------------------------------------------------------ inherit

__SPAS_ACCUM="IUSE REQUIRED_USE DEPEND RDEPEND PDEPEND BDEPEND IDEPEND"
[[ ${EAPI} != 7 ]] && __SPAS_ACCUM+=" PROPERTIES RESTRICT"

inherit() {
	local __spas_ec __spas_loc __spas_d __spas_v __spas_k __spas_f
	local -a __spas_dirs __spas_saved __spas_isset __spas_prev_export
	local __spas_prev_eclass
	IFS=: read -ra __spas_dirs <<< "${SPAS_ECLASSDIRS}"
	for __spas_ec in "$@"; do
		__spas_loc=
		for (( __spas_k = ${#__spas_dirs[@]} - 1; __spas_k >= 0; __spas_k-- )); do
			if [[ -f ${__spas_dirs[__spas_k]}/${__spas_ec}.eclass ]]; then
				__spas_loc=${__spas_dirs[__spas_k]}/${__spas_ec}.eclass
				break
			fi
		done
		[[ -n ${__spas_loc} ]] || die "${__spas_ec}.eclass could not be found by inherit"
		__spas_saved=()
		__spas_isset=()
		for __spas_v in ${__SPAS_ACCUM}; do
			if [[ -v ${__spas_v} ]]; then
				__spas_isset+=( 1 )
				__spas_saved+=( "${!__spas_v}" )
			else
				__spas_isset+=( 0 )
				__spas_saved+=( "" )
			fi
			unset "${__spas_v}"
		done
		__spas_prev_eclass=${ECLASS}
		__spas_prev_export=( "${__SPAS_EXPORT[@]}" )
		__SPAS_EXPORT=()
		export ECLASS=${__spas_ec}
		has "${__spas_ec}" ${INHERITED} || export INHERITED="${INHERITED:+${INHERITED} }${__spas_ec}"
		source "${__spas_loc}" || die "failed to source ${__spas_loc}"
		for __spas_f in "${__SPAS_EXPORT[@]}"; do
			eval "${__spas_f}() { ${__spas_ec}_${__spas_f} \"\$@\"; }"
		done
		__SPAS_EXPORT=( "${__spas_prev_export[@]}" )
		__spas_k=0
		for __spas_v in ${__SPAS_ACCUM}; do
			if [[ -v ${__spas_v} ]]; then
				local __spas_e="E_${__spas_v}"
				declare -g "${__spas_e}=${!__spas_e} ${!__spas_v}"
			fi
			if [[ ${__spas_isset[__spas_k]} == 1 ]]; then
				declare -g "${__spas_v}=${__spas_saved[__spas_k]}"
			else
				unset "${__spas_v}"
			fi
			(( __spas_k++ ))
		done
		if [[ -n ${__spas_prev_eclass} ]]; then
			export ECLASS=${__spas_prev_eclass}
		else
			unset ECLASS
		fi
	done
}

EXPORT_FUNCTIONS() {
	[[ -n ${ECLASS} ]] || die "EXPORT_FUNCTIONS called outside of an eclass"
	__SPAS_EXPORT+=( "$@" )
}

# ------------------------------------------------------------------ install helpers

__SPAS_DESTTREE=/usr
__SPAS_INSDESTTREE=/
__SPAS_EXEDESTTREE=/
__SPAS_DOCDESTTREE=
__SPAS_INSOPTS=( -m0644 )
__SPAS_EXEOPTS=( -m0755 )
__SPAS_DIROPTS=( -m0755 )

into()    { [[ $1 == / ]] && __SPAS_DESTTREE= || __SPAS_DESTTREE=$1; }
insinto() { __SPAS_INSDESTTREE=$1; }
exeinto() { __SPAS_EXEDESTTREE=$1; }
docinto() { [[ $1 == / ]] && __SPAS_DOCDESTTREE= || __SPAS_DOCDESTTREE=$1; }
insopts() { __SPAS_INSOPTS=( "$@" ); }
exeopts() { __SPAS_EXEOPTS=( "$@" ); }
diropts() { __SPAS_DIROPTS=( "$@" ); }

__spas_need_d() {
	[[ -n ${ED} && ${EBUILD_PHASE} == install ]] || die "${FUNCNAME[1]} may only be used in src_install"
}

dodir() {
	__spas_need_d
	local d
	for d in "$@"; do
		install -d "${__SPAS_DIROPTS[@]}" "${ED}/${d#/}" || __helper_die "dodir ${d} failed" || return
	done
}

keepdir() {
	__spas_need_d
	dodir "$@" || return
	local d
	for d in "$@"; do
		: > "${ED}/${d#/}/.keep_${CATEGORY}_${PN}-${SLOT%/*}" || __helper_die "keepdir ${d} failed" || return
	done
}

# __spas_inst <dest dir> <mode opts...> -- files...
__spas_inst() {
	local dest=$1
	shift
	local -a opts=()
	while [[ $1 != -- ]]; do opts+=( "$1" ); shift; done
	shift
	[[ $# -gt 0 ]] || { __helper_die "${FUNCNAME[1]}: no files given"; return; }
	install -d "${__SPAS_DIROPTS[@]}" "${dest}" || { __helper_die "cannot create ${dest}"; return; }
	local f ret=0
	for f in "$@"; do
		if [[ -L ${f} ]]; then
			cp -P "${f}" "${dest}/" || ret=1
		elif [[ -f ${f} ]]; then
			install "${opts[@]}" "${f}" "${dest}/" || ret=1
		else
			eerror "${FUNCNAME[1]}: ${f} does not exist"
			ret=1
		fi
	done
	[[ ${ret} -eq 0 ]] || __helper_die "${FUNCNAME[1]} failed"
}

dobin()  { __spas_need_d; __spas_inst "${ED}${__SPAS_DESTTREE}/bin" -m0755 -- "$@"; }
dosbin() { __spas_need_d; __spas_inst "${ED}${__SPAS_DESTTREE}/sbin" -m0755 -- "$@"; }
doexe()  { __spas_need_d; __spas_inst "${ED}/${__SPAS_EXEDESTTREE#/}" "${__SPAS_EXEOPTS[@]}" -- "$@"; }
dolib.so() { __spas_need_d; __spas_inst "${ED}${__SPAS_DESTTREE}/$(get_libdir)" -m0755 -- "$@"; }
dolib.a()  { __spas_need_d; __spas_inst "${ED}${__SPAS_DESTTREE}/$(get_libdir)" -m0644 -- "$@"; }
doinfo() { __spas_need_d; __spas_inst "${ED}/usr/share/info" -m0644 -- "$@"; }
doconfd() { __spas_need_d; __spas_inst "${ED}/etc/conf.d" -m0644 -- "$@"; }
doenvd()  { __spas_need_d; __spas_inst "${ED}/etc/env.d" -m0644 -- "$@"; }
doinitd() { __spas_need_d; __spas_inst "${ED}/etc/init.d" -m0755 -- "$@"; }

# recursive copy used by doins -r / dodoc -r / doheader -r
__spas_inst_r() {
	local dest=$1 mode=$2 src=$3
	if [[ -L ${src} ]]; then
		install -d "${__SPAS_DIROPTS[@]}" "${dest}" && cp -P "${src}" "${dest}/"
	elif [[ -d ${src} ]]; then
		local base=${src%/}
		base=${base##*/}
		install -d "${__SPAS_DIROPTS[@]}" "${dest}/${base}" || return 1
		local e
		shopt -s dotglob nullglob
		for e in "${src%/}"/*; do
			__spas_inst_r "${dest}/${base}" "${mode}" "${e}" || { shopt -u dotglob nullglob; return 1; }
		done
		shopt -u dotglob nullglob
	elif [[ -e ${src} ]]; then
		install -d "${__SPAS_DIROPTS[@]}" "${dest}" && install ${mode} "${src}" "${dest}/"
	else
		eerror "${src} does not exist"
		return 1
	fi
}

doins() {
	__spas_need_d
	local rec=0
	[[ $1 == -r ]] && { rec=1; shift; }
	[[ $# -gt 0 ]] || { __helper_die "doins: no files given"; return; }
	local dest="${ED}/${__SPAS_INSDESTTREE#/}" f ret=0
	for f in "$@"; do
		if [[ ${rec} == 1 && -d ${f} && ! -L ${f} ]]; then
			__spas_inst_r "${dest}" "${__SPAS_INSOPTS[*]}" "${f}" || ret=1
		elif [[ -d ${f} && ! -L ${f} ]]; then
			eerror "doins: ${f} is a directory (use doins -r)"
			ret=1
		else
			__spas_inst "${dest}" "${__SPAS_INSOPTS[@]}" -- "${f}" || ret=1
		fi
	done
	[[ ${ret} -eq 0 ]] || __helper_die "doins failed"
}

dodoc() {
	__spas_need_d
	local rec=0
	[[ $1 == -r ]] && { rec=1; shift; }
	[[ $# -gt 0 ]] || { __helper_die "dodoc: no files given"; return; }
	local dest="${ED}/usr/share/doc/${PF}${__SPAS_DOCDESTTREE:+/${__SPAS_DOCDESTTREE#/}}" f ret=0
	for f in "$@"; do
		if [[ -d ${f} && ! -L ${f} ]]; then
			if [[ ${rec} == 1 ]]; then
				__spas_inst_r "${dest}" -m0644 "${f}" || ret=1
			else
				eerror "dodoc: ${f} is a directory (use dodoc -r)"
				ret=1
			fi
		elif [[ -e ${f} ]]; then
			install -d "${dest}" && install -m0644 "${f}" "${dest}/" || ret=1
		else
			eerror "dodoc: ${f} does not exist"
			ret=1
		fi
	done
	[[ ${ret} -eq 0 ]] || __helper_die "dodoc failed"
}

doheader() {
	__spas_need_d
	local rec=0
	[[ $1 == -r ]] && { rec=1; shift; }
	local f ret=0
	for f in "$@"; do
		if [[ ${rec} == 1 && -d ${f} ]]; then
			__spas_inst_r "${ED}/usr/include" -m0644 "${f}" || ret=1
		else
			__spas_inst "${ED}/usr/include" -m0644 -- "${f}" || ret=1
		fi
	done
	[[ ${ret} -eq 0 ]] || __helper_die "doheader failed"
}

doman() {
	__spas_need_d
	local i18n="" f name sect lang ret=0
	if [[ $1 == -i18n=* ]]; then i18n=${1#-i18n=}; shift; fi
	for f in "$@"; do
		name=${f##*/}
		local base=${name%.gz}
		base=${base%.bz2}
		sect=${base##*.}
		if [[ ! ${sect} =~ ^[0-9n] ]]; then
			eqawarn "doman: ${f} has no man section suffix"
			ret=1
			continue
		fi
		sect=${sect:0:1}
		lang=${i18n}
		local stem=${base%.*}
		if [[ -z ${lang} && ${stem} =~ \.([a-z][a-z](_[A-Z][A-Z])?)$ ]]; then
			lang=${BASH_REMATCH[1]}
			name=${stem%.*}.${base##*.}
			[[ ${f} == *.gz ]] && name+=.gz
			[[ ${f} == *.bz2 ]] && name+=.bz2
		fi
		local dest="${ED}/usr/share/man${lang:+/${lang}}/man${sect}"
		install -d "${dest}" && install -m0644 "${f}" "${dest}/${name}" || ret=1
	done
	[[ ${ret} -eq 0 ]] || __helper_die "doman failed"
}

if [[ ${EAPI} == [78] ]]; then
	domo() {
		__spas_need_d
		local f lang ret=0
		for f in "$@"; do
			lang=${f##*/}
			lang=${lang%.mo}
			install -d "${ED}/usr/share/locale/${lang}/LC_MESSAGES" &&
				install -m0644 "${f}" "${ED}/usr/share/locale/${lang}/LC_MESSAGES/${PN}.mo" || ret=1
		done
		[[ ${ret} -eq 0 ]] || __helper_die "domo failed"
	}
fi

# new<helper> <src> <newname>: copy under the new name, then use do<helper>
__spas_new() {
	local helper=$1 src=$2 name=$3
	shift 3
	[[ -n ${name} && ${name} != */* ]] || { __helper_die "new${helper#do}: invalid name '${name}'"; return; }
	local tmp="${T}/.spas_new/${RANDOM}${RANDOM}"
	mkdir -p "${tmp}" || return 1
	if [[ ${src} == - ]]; then
		cat > "${tmp}/${name}" || { __helper_die "reading stdin failed"; return; }
	else
		cp -P "${src}" "${tmp}/${name}" || { __helper_die "new${helper#do}: ${src} does not exist"; return; }
	fi
	"${helper}" "$@" "${tmp}/${name}"
	local r=$?
	rm -rf "${tmp}"
	return ${r}
}
newbin()     { __spas_new dobin "$@"; }
newsbin()    { __spas_new dosbin "$@"; }
newins()     { __spas_new doins "$@"; }
newexe()     { __spas_new doexe "$@"; }
newdoc()     { __spas_new dodoc "$@"; }
newconfd()   { __spas_new doconfd "$@"; }
newenvd()    { __spas_new doenvd "$@"; }
newinitd()   { __spas_new doinitd "$@"; }
newheader()  { __spas_new doheader "$@"; }
newlib.so()  { __spas_new dolib.so "$@"; }
newlib.a()   { __spas_new dolib.a "$@"; }
newman() {
	if [[ $1 == -i18n=* ]]; then
		local o=$1
		shift
		__spas_new doman "$1" "$2" "${o}"
	else
		__spas_new doman "$@"
	fi
}

dosym() {
	__spas_need_d
	local rel=0
	if [[ $1 == -r ]]; then
		[[ ${EAPI} == 7 ]] && die "dosym -r requires EAPI 8"
		rel=1
		shift
	fi
	[[ $# -eq 2 ]] || { __helper_die "usage: dosym [-r] <target> <link>"; return; }
	local target=$1 link=$2
	[[ ${link} == */ ]] && { __helper_die "dosym: link '${link}' ends with /"; return; }
	if [[ ${rel} == 1 ]]; then
		[[ ${target} == /* ]] || { __helper_die "dosym -r: target must be absolute"; return; }
		local from=${link%/*}
		[[ ${link} != */* ]] && from=.
		local -a fa ta
		IFS=/ read -ra fa <<< "${from#/}"
		IFS=/ read -ra ta <<< "${target#/}"
		local i=0
		while [[ ${i} -lt ${#fa[@]} && ${i} -lt ${#ta[@]} && ${fa[i]} == "${ta[i]}" ]]; do (( i++ )); done
		local out="" j
		for (( j = i; j < ${#fa[@]}; j++ )); do [[ -n ${fa[j]} ]] && out+="../"; done
		local rest
		rest=$(IFS=/; echo "${ta[*]:i}")
		target=${out}${rest}
		[[ -z ${target} ]] && target=.
	fi
	local dir=${link%/*}
	[[ ${link} == */* ]] && { install -d "${ED}/${dir#/}" || { __helper_die "dosym failed"; return; }; }
	ln -snf "${target}" "${ED}/${link#/}" || __helper_die "dosym ${target} ${link} failed"
}

fperms() {
	__spas_need_d
	local mode=$1 f ret=0
	shift
	for f in "$@"; do chmod "${mode}" "${ED}/${f#/}" || ret=1; done
	[[ ${ret} -eq 0 ]] || __helper_die "fperms failed"
}

fowners() {
	__spas_need_d
	local -a opts=()
	while [[ $1 == -* ]]; do opts+=( "$1" ); shift; done
	local own=$1 f ret=0
	shift
	for f in "$@"; do chown "${opts[@]}" "${own}" "${ED}/${f#/}" || ret=1; done
	[[ ${ret} -eq 0 ]] || __helper_die "fowners failed"
}

docompress() {
	local list=include
	[[ $1 == -x ]] && { list=exclude; shift; }
	local f
	for f in "$@"; do printf '%s\n' "/${f#/}" >> "${T}/.spas_docompress_${list}"; done
}

dostrip() {
	local list=include
	[[ $1 == -x ]] && { list=exclude; shift; }
	local f
	for f in "$@"; do printf '%s\n' "/${f#/}" >> "${T}/.spas_dostrip_${list}"; done
}

einstalldocs() {
	local d
	if [[ $(declare -p DOCS 2>/dev/null) == "declare -a"* ]]; then
		[[ ${#DOCS[@]} -gt 0 ]] && { docinto /; dodoc -r "${DOCS[@]}" || die "einstalldocs failed"; }
	elif [[ -v DOCS ]]; then
		[[ -n ${DOCS} ]] && { docinto /; dodoc -r ${DOCS} || die "einstalldocs failed"; }
	else
		docinto /
		for d in README* ChangeLog AUTHORS NEWS TODO CHANGES THANKS BUGS FAQ CREDITS CHANGELOG; do
			[[ -f ${d} && -s ${d} ]] && dodoc "${d}"
		done
	fi
	if [[ $(declare -p HTML_DOCS 2>/dev/null) == "declare -a"* ]]; then
		[[ ${#HTML_DOCS[@]} -gt 0 ]] && { docinto html; dodoc -r "${HTML_DOCS[@]}" || die "einstalldocs failed"; }
	elif [[ -n ${HTML_DOCS} ]]; then
		docinto html
		dodoc -r ${HTML_DOCS} || die "einstalldocs failed"
	fi
	docinto /
	return 0
}

# ------------------------------------------------------------------ build helpers

emake() {
	local -a mopts
	read -ra mopts <<< "${MAKEOPTS}"
	set -- ${MAKE:-make} "${mopts[@]}" ${EXTRA_EMAKE} "$@"
	[[ -n ${SPAS_VERBOSE} ]] && einfo "$*"
	"$@" || __helper_die "emake failed"
}

econf() {
	local conf=${ECONF_SOURCE:-.}/configure
	[[ -x ${conf} ]] || { __helper_die "econf: ${conf} not found or not executable"; return; }
	# refresh ancient config.guess/config.sub when gnuconfig is installed
	if [[ -d /usr/share/gnuconfig ]]; then
		local x
		while IFS= read -r -d '' x; do
			cp -f "/usr/share/gnuconfig/${x##*/}" "${x}" 2>/dev/null
		done < <(find "${WORKDIR}" \( -name config.guess -o -name config.sub \) -type f -print0 2>/dev/null)
	fi
	local help
	help=$("${conf}" --help 2>/dev/null)
	local -a args=(
		--prefix="${EPREFIX}"/usr
		${CBUILD:+--build=${CBUILD}}
		--host="${CHOST}"
		${CTARGET:+--target=${CTARGET}}
		--mandir="${EPREFIX}"/usr/share/man
		--infodir="${EPREFIX}"/usr/share/info
		--datadir="${EPREFIX}"/usr/share
		--sysconfdir="${EPREFIX}"/etc
		--localstatedir="${EPREFIX}"/var/lib
	)
	[[ ${EAPI} != 7 ]] && args+=( --datarootdir="${EPREFIX}"/usr/share )
	if [[ " $* " != *" --libdir="* ]]; then
		args+=( --libdir="${EPREFIX}/usr/$(get_libdir)" )
	fi
	[[ ${help} == *--disable-dependency-tracking* ]] && args+=( --disable-dependency-tracking )
	[[ ${help} == *--disable-silent-rules* ]] && args+=( --disable-silent-rules )
	[[ ${help} == *--docdir* ]] && args+=( --docdir="${EPREFIX}/usr/share/doc/${PF}" )
	[[ ${help} == *--htmldir* ]] && args+=( --htmldir="${EPREFIX}/usr/share/doc/${PF}/html" )
	[[ ${help} == *--with-sysroot* ]] && args+=( --with-sysroot="${ESYSROOT:-/}" )
	if [[ ${EAPI} != 7 && ${help} == *--enable-static* && " $* " != *" --enable-static"* && " $* " != *" --disable-static"* ]]; then
		args+=( --disable-static )
	fi
	einfo "${conf} ${args[*]} $*"
	"${conf}" "${args[@]}" "$@" || {
		[[ -f config.log ]] && eerror "Please check ${PWD}/config.log"
		__helper_die "econf failed"
		return
	}
}

unpack() {
	[[ $# -gt 0 ]] || die "unpack: no files given"
	local x src name sfx
	for x in "$@"; do
		if [[ ${x} == ./* || ${x} == /* ]]; then
			src=${x}
		elif [[ ${x} == */* ]]; then
			src=./${x}
		else
			src=${DISTDIR}/${x}
		fi
		[[ -s ${src} ]] || die "unpack: ${src} does not exist or is empty"
		name=${x##*/}
		einfo "Unpacking ${name} to ${PWD}"
		case ${name} in
			*.tar)
				tar xof "${src}" ;;
			*.tar.gz|*.tgz|*.tar.Z|*.tar.z)
				gzip -dc "${src}" | tar xof - ; pipestatus ;;
			*.tar.bz2|*.tbz2|*.tbz)
				bzip2 -dc "${src}" | tar xof - ; pipestatus ;;
			*.tar.xz|*.txz)
				xz -dc "${src}" | tar xof - ; pipestatus ;;
			*.tar.lzma)
				xz --format=lzma -dc "${src}" | tar xof - ; pipestatus ;;
			*.tar.lz)
				lzip -dc "${src}" | tar xof - ; pipestatus ;;
			*.tar.zst)
				zstd -dc "${src}" | tar xof - ; pipestatus ;;
			*.gz|*.Z|*.z)
				gzip -dc "${src}" > "${name%.*}" ;;
			*.bz2|*.bz)
				bzip2 -dc "${src}" > "${name%.*}" ;;
			*.xz)
				xz -dc "${src}" > "${name%.*}" ;;
			*.lzma)
				xz --format=lzma -dc "${src}" > "${name%.*}" ;;
			*.lz)
				lzip -dc "${src}" > "${name%.*}" ;;
			*.zst)
				zstd -dc "${src}" > "${name%.*}" ;;
			*.zip|*.ZIP|*.jar)
				unzip -qo "${src}" ;;
			*.7z|*.7Z)
				[[ ${EAPI} == 7 ]] || die "unpack: .7z is not supported in EAPI ${EAPI}"
				7z x -y "${src}" >/dev/null ;;
			*.rar|*.RAR)
				[[ ${EAPI} == 7 ]] || die "unpack: .rar is not supported in EAPI ${EAPI}"
				unrar x -idq -o+ "${src}" ;;
			*.lha|*.LHa|*.LHA|*.lzh)
				[[ ${EAPI} == 7 ]] || die "unpack: .lha is not supported in EAPI ${EAPI}"
				lha xfq "${src}" ;;
			*.deb)
				ar x "${src}" ;;
			*.a)
				ar x "${src}" ;;
			*)
				eqawarn "unpack: skipping unrecognized file ${name}"
				continue ;;
		esac || die "unpack: failed to unpack ${name}"
	done
	chmod -fR a+rX,u+w,g-w,o-w . 2>/dev/null
	return 0
}

eapply() {
	local -a opts=() files=()
	local seen_dd=0 x
	for x in "$@"; do
		if [[ ${seen_dd} == 0 && ${x} == -- ]]; then
			seen_dd=1
		elif [[ ${seen_dd} == 0 && ${x} == -* ]]; then
			opts+=( "${x}" )
		else
			files+=( "${x}" )
		fi
	done
	[[ ${#files[@]} -gt 0 ]] || die "eapply: no patches given"
	local f p
	for f in "${files[@]}"; do
		if [[ -d ${f} ]]; then
			local -a ps=()
			shopt -s nullglob
			ps=( "${f%/}"/*.diff "${f%/}"/*.patch )
			shopt -u nullglob
			[[ ${#ps[@]} -gt 0 ]] || die "eapply: no *.diff or *.patch files in ${f}"
			local IFS=$'\n'
			ps=( $(printf '%s\n' "${ps[@]}" | LC_ALL=C sort) )
			unset IFS
			for p in "${ps[@]}"; do
				__spas_patch "${p}" "${opts[@]}" || return 1
			done
		else
			__spas_patch "${f}" "${opts[@]}" || return 1
		fi
	done
}

__spas_patch() {
	local p=$1
	shift
	[[ -f ${p} ]] || { __helper_die "eapply: ${p} does not exist"; return; }
	einfo "Applying ${p##*/} ..."
	local out
	out=$(LC_ALL=C patch -p1 -f -g0 --no-backup-if-mismatch "$@" < "${p}" 2>&1)
	if [[ $? -ne 0 ]]; then
		printf '%s\n' "${out}" >&2
		__helper_die "eapply: ${p##*/} failed to apply"
		return
	fi
	return 0
}

eapply_user() {
	[[ ${EBUILD_PHASE} == prepare ]] || die "eapply_user may only be called in src_prepare"
	[[ -f ${T}/.spas_eapply_user ]] && return 0
	: > "${T}/.spas_eapply_user"
	local base=${PORTAGE_CONFIGROOT%/}/etc/portage/patches
	local d slot=${SLOT%/*}
	local -a cands=(
		"${CATEGORY}/${PN}-${PV}-${PR}" "${CATEGORY}/${PN}-${PV}-${PR}:${slot}"
		"${CATEGORY}/${PN}-${PV}" "${CATEGORY}/${PN}-${PV}:${slot}"
		"${CATEGORY}/${PN}" "${CATEGORY}/${PN}:${slot}"
	)
	for d in "${cands[@]}"; do
		if [[ -d ${base}/${d} ]]; then
			local -a ps=()
			shopt -s nullglob
			ps=( "${base}/${d}"/*.diff "${base}/${d}"/*.patch )
			shopt -u nullglob
			if [[ ${#ps[@]} -gt 0 ]]; then
				einfo "Applying user patches from ${base}/${d} ..."
				eapply "${ps[@]}"
				einfo "User patches applied."
			fi
			break
		fi
	done
}

addread()    { export SANDBOX_READ="${SANDBOX_READ:+${SANDBOX_READ}:}$1"; }
addwrite()   { export SANDBOX_WRITE="${SANDBOX_WRITE:+${SANDBOX_WRITE}:}$1"; }
addpredict() { export SANDBOX_PREDICT="${SANDBOX_PREDICT:+${SANDBOX_PREDICT}:}$1"; }
adddeny()    { export SANDBOX_DENY="${SANDBOX_DENY:+${SANDBOX_DENY}:}$1"; }

# ------------------------------------------------------------------ default phases

default_pkg_nofetch() {
	[[ -n ${A} ]] || return 0
	eerror "The following files cannot be fetched for ${PN}:"
	local f
	for f in ${A}; do
		[[ -f ${DISTDIR}/${f} ]] || eerror "   ${f}"
	done
}

default_src_unpack() {
	[[ -n ${A} ]] && unpack ${A}
	return 0
}

default_src_prepare() {
	if [[ $(declare -p PATCHES 2>/dev/null) == "declare -a"* ]]; then
		[[ ${#PATCHES[@]} -gt 0 ]] && eapply "${PATCHES[@]}"
	elif [[ -n ${PATCHES} ]]; then
		eapply ${PATCHES}
	fi
	eapply_user
}

default_src_configure() {
	if [[ -x ${ECONF_SOURCE:-.}/configure ]]; then
		econf
	fi
}

default_src_compile() {
	if [[ -f Makefile || -f GNUmakefile || -f makefile ]]; then
		emake || die "emake failed"
	fi
}

default_src_test() {
	if [[ -f Makefile || -f GNUmakefile || -f makefile ]]; then
		if ${MAKE:-make} -n check >/dev/null 2>&1; then
			emake check || die "make check failed"
		elif ${MAKE:-make} -n test >/dev/null 2>&1; then
			emake test || die "make test failed"
		fi
	fi
}

default_src_install() {
	if [[ -f Makefile || -f GNUmakefile || -f makefile ]]; then
		emake DESTDIR="${D}" install
	fi
	einstalldocs
}

default() {
	if declare -F "default_${EBUILD_PHASE_FUNC}" >/dev/null; then
		"default_${EBUILD_PHASE_FUNC}"
	else
		die "default is not available in ${EBUILD_PHASE_FUNC}"
	fi
}

# ------------------------------------------------------------------ driver internals

__SPAS_PHASES="pkg_pretend pkg_setup src_unpack src_prepare src_configure src_compile src_test src_install pkg_preinst pkg_postinst pkg_prerm pkg_postrm pkg_config pkg_info pkg_nofetch"

# variables never written to a saved environment
__spas_skip_var() {
	case $1 in
		BASH|BASH_*|BASHOPTS|BASHPID|COMP_*|DIRSTACK|EUID|UID|PPID|FUNCNAME|GROUPS|HISTCMD|HOSTNAME|HOSTTYPE|IFS|LINENO|LINES|COLUMNS|MACHTYPE|OPTERR|OPTIND|OSTYPE|PIPESTATUS|PS1|PS2|PS3|PS4|PWD|OLDPWD|RANDOM|SECONDS|SHELLOPTS|SHLVL|SRANDOM|EPOCHREALTIME|EPOCHSECONDS|_|__spas_*|__SPAS_USE|__SPAS_IUSE_EFF|__SPAS_VC|SPAS_*|SANDBOX_*|LD_PRELOAD|BASH_COMPAT) return 0 ;;
	esac
	has "$1" ${__SPAS_FRESH} && return 0
	return 1
}

# variables that always come from hoard, never from a saved environment
__SPAS_FRESH="EBUILD_PHASE EBUILD_PHASE_FUNC REPLACING_VERSIONS REPLACED_BY_VERSION ROOT EROOT SYSROOT ESYSROOT BROOT EPREFIX D ED T WORKDIR HOME TMPDIR DISTDIR PORTAGE_ACTUAL_DISTDIR PORTAGE_BUILDDIR PORTAGE_TMPDIR PORTAGE_CONFIGROOT FILESDIR EBUILD MERGE_TYPE PATH USE TERM NOCOLOR PORTAGE_REPO_NAME A"

__spas_save_env() {
	local __n __d __flags
	for __n in $(compgen -v); do
		__spas_skip_var "${__n}" && continue
		__d=$(declare -p "${__n}" 2>/dev/null) || continue
		# drop the readonly attribute so the file can be sourced again
		if [[ ${__d} =~ ^declare\ -([a-zA-Z]+)\  ]]; then
			__flags=${BASH_REMATCH[1]//r/}
			__d="declare -${__flags:--} ${__d#declare -${BASH_REMATCH[1]} }"
		fi
		printf '%s\n' "${__d}"
	done
	local __f
	for __f in $(compgen -A function); do
		[[ ${__f} == __spas_* || ${__f} == __helper_die ]] && continue
		has "${__f}" ${__SPAS_LIB_FUNCS} && continue
		declare -f "${__f}"
	done
}

__spas_run_phase() {
	local phase=$1 fn
	case ${phase} in
		pretend|setup|preinst|postinst|prerm|postrm|config|info|nofetch) fn=pkg_${phase} ;;
		*) fn=src_${phase} ;;
	esac
	export EBUILD_PHASE_FUNC=${fn}
	__spas_build_use_set
	case ${phase} in
		unpack) cd "${WORKDIR}" || die "cannot cd to WORKDIR" ;;
		prepare|configure|compile|test|install)
			if [[ -d ${S} ]]; then
				cd "${S}" || die "cannot cd to S"
			elif [[ -z ${A} || ${S} == "${WORKDIR}" ]]; then
				cd "${WORKDIR}" || die "cannot cd to WORKDIR"
			else
				die "The source directory '${S}' doesn't exist"
			fi ;;
		*) cd "${WORKDIR:-${T}}" 2>/dev/null || cd "${T}" 2>/dev/null || cd / ;;
	esac
	declare -F "pre_${fn}" >/dev/null && "pre_${fn}"
	if declare -F "${fn}" >/dev/null; then
		"${fn}"
	elif declare -F "default_${fn}" >/dev/null; then
		"default_${fn}"
	fi
	if [[ ${phase} == prepare && ! -f ${T}/.spas_eapply_user ]]; then
		die "eapply_user (or default) must be called in src_prepare()"
	fi
	declare -F "post_${fn}" >/dev/null && "post_${fn}"
	return 0
}

__spas_metadata() {
	local k v out=""
	for k in ${__SPAS_ACCUM}; do
		local e="E_${k}"
		[[ -n ${!e} ]] && declare -g "${k}=${!k} ${!e}"
	done
	local defined=() p
	for p in ${__SPAS_PHASES}; do
		declare -F "${p}" >/dev/null && defined+=( "${p#*_}" )
	done
	local dp
	dp=$(printf '%s\n' "${defined[@]}" | LC_ALL=C sort | tr '\n' ' ')
	dp=${dp% }
	DEFINED_PHASES=${dp:--}
	INHERIT=${INHERITED}
	for k in DEPEND RDEPEND BDEPEND PDEPEND IDEPEND SLOT SRC_URI RESTRICT HOMEPAGE LICENSE DESCRIPTION KEYWORDS INHERITED IUSE REQUIRED_USE EAPI PROPERTIES DEFINED_PHASES; do
		v=${!k}
		v=$(printf '%s' "${v}" | tr '\n\t' '  ')
		v=$(echo ${v})
		out+="${k}=${v}"$'\n'
	done
	printf '%s' "${out}" > "${SPAS_META_OUT}" || die "cannot write metadata"
}

__SPAS_LIB_FUNCS=$(compgen -A function | tr '\n' ' ')

# ------------------------------------------------------------------ driver (top level on purpose:
# the saved environment and the ebuild must be sourced in global scope)

case ${SPAS_MODE} in
depend|phase)
	if [[ ${SPAS_MODE} == phase && -n ${SPAS_ENV_IN} && -f ${SPAS_ENV_IN} ]]; then
		__spas_fresh=$(declare -p ${__SPAS_FRESH} $(compgen -v SPAS_) 2>/dev/null)
		source "${SPAS_ENV_IN}" || die "failed to load the saved environment"
		eval "${__spas_fresh}"
		unset __spas_fresh
	else
		unset DEPEND RDEPEND BDEPEND PDEPEND IDEPEND IUSE REQUIRED_USE SLOT SRC_URI RESTRICT PROPERTIES \
			HOMEPAGE LICENSE DESCRIPTION KEYWORDS INHERITED E_IUSE E_REQUIRED_USE E_DEPEND E_RDEPEND \
			E_BDEPEND E_PDEPEND E_IDEPEND E_PROPERTIES E_RESTRICT
		__SPAS_EXPORT=()
		export S=${WORKDIR}/${P}
		shopt -s failglob
		source "${EBUILD}" || die "error sourcing ${EBUILD}"
		shopt -u failglob
		if [[ ${SPAS_MODE} == depend ]]; then
			__spas_metadata
			exit 0
		fi
		for __spas_k in ${__SPAS_ACCUM}; do
			__spas_e="E_${__spas_k}"
			[[ -n ${!__spas_e} ]] && declare -g "${__spas_k}=${!__spas_k} ${!__spas_e}"
		done
		unset __spas_k __spas_e
	fi
	# bashrc hooks (profiles, then /etc/portage/bashrc)
	if [[ -n ${SPAS_BASHRCS} ]]; then
		IFS=: read -ra __spas_rcs <<< "${SPAS_BASHRCS}"
		for __spas_rc in "${__spas_rcs[@]}"; do
			[[ -f ${__spas_rc} ]] && source "${__spas_rc}"
		done
		unset __spas_rcs __spas_rc
	fi
	__spas_run_phase "${EBUILD_PHASE}" || die "phase ${EBUILD_PHASE} failed"
	if [[ -n ${SPAS_ENV_OUT} ]]; then
		__spas_save_env > "${SPAS_ENV_OUT}" || die "cannot save the environment"
	fi
	exit 0
	;;
esac
