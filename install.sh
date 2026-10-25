#!/bin/sh
# Muu IME installs the ZIP and SHA-256 from GitHub Releases. The app is ad-hoc
# signed, without Developer ID or notarization. SHA-256 detects corruption and
# mismatched assets; it is not an independent signature of the publisher.
# Trust includes this script's repository and the release account. Compromise
# of both the ZIP and its checksum is outside this verification.
#
# What the checks below can and cannot say: the SHA-256 ties the ZIP to the
# checksum published in the same release; codesign --verify proves the bundle
# bytes are the ones that were ad-hoc signed, not who signed them; the plist
# and Mach-O checks reject a different product, variant or CPU. Nothing here
# replaces notarization, so an installed copy is not vetted by Apple, and a
# copy without quarantine is still not promised to pass every launch policy.
#
# Recovery promise: at any exit, including an interrupt, an install never
# leaves an unverified app in the destination, and the previous app always
# survives in the destination, the stage directory or Trash. When restoring
# it fails, it stays in the stage and the script prints where. Uninstall
# leaves the destination empty with the app in Trash or the stage.
# The previous app is never deleted here. It moves to Trash only after the new
# one is verified, and otherwise stays in the stage directory, which cleanup
# then keeps. A crash or power loss skips cleanup; the stage still holds it.

# Define everything before invoking it: an interrupted curl stream must not
# start installation while the function definitions are still incomplete.
muu_fail() {
    printf 'Muu IME: %s\n' "$*" >&2
    exit 1
}

muu_present() { [ -e "$1" ] || [ -L "$1" ]; }

# Measured sizes are a 54,108,901-byte ZIP and a 173,314,903-byte app:
# 132 files and 242 ZIP entries including directories. The caps leave about
# 2.48x headroom for compressed bytes (128 MiB), 3.10x for expanded logical
# bytes (512 MiB), and 4.23x for all ZIP entries (1,024), so ordinary growth
# fits while limiting the resources consumed by a corrupt or hostile ZIP.
# The download cap applies to both files; SHA256SUMS has its own 4 KiB cap.
muu_zip_limit=134217728
muu_expanded_limit=536870912
muu_entry_limit=1024

muu_no_links() (
    probe=$1
    while [ "$probe" != / ]; do
        [ ! -L "$probe" ] || exit 1
        probe=${probe%/*}
        [ -n "$probe" ] || probe=/
    done
)

# Identity is device:inode, taken from the staged copy before it is renamed
# into place. A path is not enough: if another process creates the
# destination between the absence check and mv, mv moves our app inside that
# directory instead of replacing it, and the path then names someone else's
# directory. Matching the inode tells our copy from anything else there.
muu_matches_identity() {
    [ -n "$2" ] && muu_no_links "$1" && [ -d "$1" ] &&
        [ "$(stat -f '%d:%i' "$1")" = "$2" ]
}

# Shown when the lock is held: a stage left by an interrupted run may hold the
# only previous app, so the user is told where it is before removing the lock.
muu_report_stages() {
    for muu_leftover in "$muu_directory"/.muu-stage.*; do
        if muu_present "$muu_leftover"; then
            printf '退避または未完了の作業領域: %s\n' "$muu_leftover" >&2
        fi
    done
}

muu_version() {
    printf '%s\n' "$1" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$'
}

muu_os_supports() {
    awk -v current="$1" -v minimum="$2" 'BEGIN {
        if (current !~ /^[0-9]+\.[0-9]+(\.[0-9]+)?$/ ||
            minimum !~ /^[0-9]+\.[0-9]+(\.[0-9]+)?$/) exit 1;
        split(current, c, "."); split(minimum, m, ".");
        for (i = 1; i <= 3; i++) {
            if (c[i]+0 > m[i]+0) exit 0;
            if (c[i]+0 < m[i]+0) exit 1;
        }
    }'
}

muu_plist() {
    plutil -extract "$2" raw -o - "$1/Contents/Info.plist"
}

# Read-only checks of one bundle; the app is never executed or re-signed.
# Each copy (extracted, staged, installed) is checked again, and the staged
# and installed copies must also match the extracted one byte for byte, so a
# copy or rename cannot substitute other content between checks. A
# quarantine attribute is refused because the copies are made with --noqtn;
# an unnotarized copy carrying it may be refused when macOS launches it.
muu_verify_app() (
    app=$1
    label=$2
    [ -d "$app" ] && [ ! -L "$app" ] || exit 1
    find "$app" ! -type d ! -type f > "$muu_work/nonregular-$label" || exit 1
    [ ! -s "$muu_work/nonregular-$label" ] || exit 1
    [ -f "$app/Contents/Info.plist" ] || exit 1
    executable=$app/Contents/MacOS/MuuIME
    [ -f "$executable" ] && [ -x "$executable" ] || exit 1
    [ "$(muu_plist "$app" CFBundleIdentifier)" = dev.muu.inputmethod.Muu ] || exit 1
    [ "$(muu_plist "$app" CFBundleExecutable)" = MuuIME ] || exit 1
    [ "$(muu_plist "$app" CFBundlePackageType)" = APPL ] || exit 1
    [ "$(muu_plist "$app" MuuBuildVariant)" = standard ] || exit 1
    [ "$(muu_plist "$app" CFBundleShortVersionString)" = "$muu_selected_version" ] || exit 1
    build=$(muu_plist "$app" CFBundleVersion) || exit 1
    printf '%s\n' "$build" | grep -Eq '^[1-9][0-9]*$' || exit 1
    source=$(muu_plist "$app" MuuSourceCommit) || exit 1
    printf '%s\n' "$source" | grep -Eq '^[0-9a-f]{40}$' || exit 1
    minimum=$(muu_plist "$app" LSMinimumSystemVersion) || exit 1
    muu_os_supports "$minimum" 15.0 || exit 1
    muu_os_supports "$muu_os" "$minimum" || exit 1
    size=$(wc -c < "$executable") || exit 1
    [ "$size" -ge 32 ] || exit 1
    header=$(od -An -tx1 -N16 "$executable" | tr -d ' \n') || exit 1
    # Thin little-endian 64-bit Mach-O, ARM64_ALL, MH_EXECUTE. This is only
    # the CPU/type check; the release gate checks load commands and minos.
    [ "$header" = cffaedfe0c0000010000000002000000 ] || exit 1
    codesign --verify --deep --strict --verbose=2 "$app" || exit 1
    codesign -d --verbose=4 "$app" > "$muu_work/signature-$label" 2>&1 || exit 1
    [ "$(grep -cx 'Signature=adhoc' "$muu_work/signature-$label")" = 1 ] || exit 1
    sed -n 's/^CDHash=//p' "$muu_work/signature-$label" > "$muu_work/cdhash-$label" || exit 1
    [ "$(wc -l < "$muu_work/cdhash-$label" | tr -d ' ')" = 1 ] || exit 1
    grep -Eq '^[0-9a-f]{40}$' "$muu_work/cdhash-$label" || exit 1
    shasum -a 256 "$executable" | awk '{print $1}' > "$muu_work/sha-$label" || exit 1
    xattr -r "$app" > "$muu_work/xattrs-$label" || exit 1
    if grep -q 'com.apple.quarantine' "$muu_work/xattrs-$label"; then exit 1; fi
)

muu_same_app() {
    muu_verify_app "$1" "$2" &&
        cmp -s "$muu_work/sha-source" "$muu_work/sha-$2" &&
        cmp -s "$muu_work/cdhash-source" "$muu_work/cdhash-$2" &&
        diff -r "$muu_source" "$1" > /dev/null
}

muu_archive_safe() {
    zipinfo -l "$1" > "$muu_work/archive-list" || return 1
    # zipinfo escapes control characters. Only the ASCII names used by the
    # release bundle are accepted; unknown listing formats fail closed.
    awk -v limit="$muu_expanded_limit" -v entry_limit="$muu_entry_limit" '
        NR == 1 && /^Archive: / {next}
        /^Zip file size: [0-9]+ bytes, number of entries: [0-9]+$/ {
            expected=$NF; headers++;
            if (expected > entry_limit) exit 1;
            next
        }
        /^[0-9]+ files?, / {summaries++; summary=$1; next}
        {
            if (NF != 10 || $1 !~ /^[-d][rwxstST-]{9}$/ || $3 != "unx") exit 1;
            if ($4 !~ /^[0-9]+$/) exit 1;
            expanded += $4;
            if (expanded > limit) exit 1;
            name=$10;
            if (name !~ /^MuuIME[.]app\// || name ~ /[^A-Za-z0-9_.\/-]/) exit 1;
            directory=substr($1,1,1)=="d";
            if (directory != (substr(name,length(name),1)=="/")) exit 1;
            if (directory) name=substr(name,1,length(name)-1);
            parts=split(name, pieces, "/");
            for (i=1;i<=parts;i++) if (pieces[i]=="" || pieces[i]=="." || pieces[i]=="..") exit 1;
            key=tolower(name);
            if (seen[key]++) exit 1;
            types[name]=directory; entries++;
        }
        END {
            if (headers != 1 || summaries != 1 || entries < 1 ||
                entries != expected || summary != expected) exit 1;
            for (name in types) {
                parent=name;
                while (sub(/\/[^\/]+$/, "", parent)) if (!(parent in types) || !types[parent]) exit 1;
            }
        }
    ' "$muu_work/archive-list" || return 1
    # Test without extracting as well: a central-directory listing alone does
    # not detect mismatched local filenames or corrupt compressed entries.
    unzip -t "$1" > "$muu_work/archive-test" 2>&1
}

# The listing caps declared sizes before inflation; this caps actual logical
# bytes afterward as well, including sparse files that du would undercount.
muu_expanded_safe() {
    find "$1" -type f -exec stat -f %z {} + > "$muu_work/expanded-sizes" || return 1
    awk -v limit="$muu_expanded_limit" '
        !/^[0-9]+$/ {exit 1}
        {total += $1; if (total > limit) exit 1}
    ' "$muu_work/expanded-sizes"
}

# HTTPS only, including redirects, with fixed timeouts and no retry. The
# installer talks to the network only when the user runs it.
muu_fetch() {
    curl -q -fsSL --proto '=https' --proto-redir '=https' --connect-timeout 15 \
        --max-time 900 --max-redirs 10 --max-filesize "$3" --output "$2" "$1" || return 1
    # Check the resulting file too, independently of curl's transfer limit.
    [ "$(wc -c < "$2")" -le "$3" ] || {
        printf 'Muu IME: download size limit exceeded: %s\n' "$2" >&2
        return 1
    }
}

# Called only after the new app is verified (or, for uninstall, after the
# app is out of the destination). The previous app goes to Trash only when a
# plain rename on the same volume can move it there; otherwise the stage is
# kept and its path is printed, never copied and deleted.
muu_retire_previous() {
    muu_present "$muu_stage/previous-MuuIME.app" || return 0
    if muu_no_links "$HOME/.Trash" && { [ -d "$HOME/.Trash" ] || mkdir "$HOME/.Trash"; } &&
        [ "$(stat -f %d "$HOME/.Trash")" = "$(stat -f %d "$muu_stage")" ]; then
        muu_trash=$HOME/.Trash/MuuIME-previous-${muu_stage##*/}.app
        if ! muu_present "$muu_trash"; then
            if mv "$muu_stage/previous-MuuIME.app" "$muu_trash"; then
                printf '旧アプリをゴミ箱に移しました: %s\n' "$muu_trash"
                return 0
            elif ! muu_present "$muu_stage/previous-MuuIME.app" && muu_present "$muu_trash"; then
                printf '移動コマンドは失敗しましたが、旧アプリはゴミ箱にあります: %s\n' "$muu_trash" >&2
                return 0
            fi
        fi
    fi
    muu_keep_stage=1
    printf '旧アプリを退避しています: %s\n' "$muu_stage/previous-MuuIME.app" >&2
}

# Runs at every exit. Before verification it undoes a partial replacement:
# an unverified new copy leaves the destination for the stage, and the
# previous app is moved back. A failed move or a quarantined copy keeps the
# stage, whose path is reported for inspection and manual cleanup.
muu_cleanup() {
    muu_status=$1
    trap - 0 1 2 15
    if [ "$muu_verified" = 0 ] && [ -n "$muu_stage" ]; then
        if [ "$muu_new_started" = 1 ] && muu_present "$muu_target"; then
            muu_rejected=
            if muu_matches_identity "$muu_target" "$muu_new_identity"; then
                muu_rejected=$muu_target
            elif muu_matches_identity "$muu_target/new-MuuIME.app" "$muu_new_identity"; then
                # mv nested our app in a directory created by another process.
                # Quarantine our copy; that other directory must survive.
                muu_rejected=$muu_target/new-MuuIME.app
            else
                # Not our inode: leave it as found. It belongs to another
                # process or the user, and moving or deleting it could destroy
                # their data. The stage is kept because the previous app may
                # not fit back into an occupied destination.
                muu_keep_stage=1
                printf '配置先が変わりました。変更せず残しています: %s\n' "$muu_target" >&2
            fi
            if [ -n "$muu_rejected" ] && ! mv "$muu_rejected" "$muu_stage/rejected-MuuIME.app"; then
                muu_keep_stage=1
                printf '未検証コピーを退避できません: %s\n' "$muu_target" >&2
            fi
        fi
        if muu_present "$muu_stage/previous-MuuIME.app"; then
            if muu_present "$muu_target" || ! mv "$muu_stage/previous-MuuIME.app" "$muu_target"; then
                muu_keep_stage=1
                printf '旧アプリを復元できません。退避先: %s\n' "$muu_stage" >&2
            fi
        fi
    fi
    # A signal may arrive after verification but before Trash receives the
    # previous app. Cleanup must never delete a stage that still holds it.
    if [ -n "$muu_stage" ] && [ "$muu_keep_stage" = 0 ] && muu_present "$muu_stage/previous-MuuIME.app"; then
        muu_keep_stage=1
        printf '旧アプリを退避しています: %s\n' "$muu_stage/previous-MuuIME.app" >&2
    fi
    # Never delete a failed quarantine. Another same-user process can replace
    # it after any stat, even inside the private stage. Identity only selects
    # a diagnostic here; it never authorizes recursive deletion of that stage.
    if [ -n "$muu_stage" ] && [ "$muu_verified" = 0 ] && muu_present "$muu_stage/rejected-MuuIME.app"; then
        muu_keep_stage=1
        if ! muu_matches_identity "$muu_stage/rejected-MuuIME.app" "$muu_new_identity"; then
            printf '退避後の配置先が変わりました。変更せず残しています: %s\n' "$muu_stage/rejected-MuuIME.app" >&2
        fi
        printf '失敗した入れ替えの作業領域を残しています: %s\n' "$muu_stage" >&2
    fi
    if [ -n "$muu_stage" ] && [ "$muu_keep_stage" = 0 ]; then
        if ! rm -r "$muu_stage"; then
            printf '作業領域が残っています: %s\n' "$muu_stage" >&2
            [ "$muu_status" != 0 ] || muu_status=1
        fi
    fi
    if [ -n "$muu_work" ]; then
        if ! rm -r "$muu_work"; then
            printf '取得用の作業領域が残っています: %s\n' "$muu_work" >&2
            [ "$muu_status" != 0 ] || muu_status=1
        fi
    fi
    if [ "$muu_have_lock" = 1 ]; then
        if ! rmdir "$muu_lock"; then
            printf 'ロックが残っています: %s\n' "$muu_lock" >&2
            [ "$muu_status" != 0 ] || muu_status=1
        fi
    fi
    exit "$muu_status"
}

muu_main() {
    umask 077
    LC_ALL=C
    export LC_ALL
    muu_version_arg=
    muu_operation=install
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --version)
                [ "$#" -ge 2 ] && [ -z "$muu_version_arg" ] && [ "$muu_operation" = install ] || muu_fail 'invalid --version'
                muu_version_arg=${2#v}
                muu_version "$muu_version_arg" || muu_fail 'invalid version'
                shift 2 ;;
            --uninstall)
                [ "$muu_operation" = install ] && [ -z "$muu_version_arg" ] || muu_fail 'conflicting arguments'
                muu_operation=uninstall
                shift ;;
            --help)
                printf 'Usage: sh install.sh [--version 0.1.0 | --uninstall]\n'
                return 0 ;;
            *) muu_fail 'unknown argument' ;;
        esac
    done
    [ "$(uname -s)" = Darwin ] || muu_fail 'macOS is required'
    [ "$(sysctl -n hw.optional.arm64)" = 1 ] || muu_fail 'Apple Silicon is required'
    muu_os=$(sw_vers -productVersion) || muu_fail 'cannot read macOS version'
    muu_os_supports "$muu_os" 15.0 || muu_fail 'macOS 15 or later is required'
    case "${HOME-}" in /*) ;; *) muu_fail 'HOME must be absolute' ;; esac
    [ "$HOME" != / ] && [ -d "$HOME" ] || muu_fail 'HOME must be a directory'
    # Destination paths are never canonicalized through links. This includes
    # missing, relative and dangling links all the way up to the filesystem root.
    muu_directory=$HOME/Library/Input\ Methods
    muu_target=$muu_directory/MuuIME.app
    muu_no_links "$muu_target" || muu_fail 'symlink in destination path'
    for muu_parent in "$HOME/Library" "$muu_directory" "$muu_target"; do
        if muu_present "$muu_parent" && [ ! -d "$muu_parent" ]; then muu_fail 'destination is not a directory'; fi
    done
    if muu_present "$muu_target"; then
        [ "$(muu_plist "$muu_target" CFBundleIdentifier)" = dev.muu.inputmethod.Muu ] || muu_fail 'destination is not Muu IME'
    fi
    # The override exists only for the test fixture; the real release URL and
    # the reserved .invalid test host are the only accepted values.
    muu_base=${MUU_INSTALL_RELEASE_BASE_URL-https://github.com/naoto256/muu-ime/releases}
    if [ "$muu_operation" = install ]; then
        case "$muu_base" in
            https://github.com/naoto256/muu-ime/releases|https://muu-installer-test.invalid/releases) ;;
            *) muu_fail 'unsupported release URL' ;;
        esac
    fi
    # State for cleanup. muu_verified: the destination now holds the verified
    # new app (or uninstall has removed it), so cleanup must not roll back.
    # muu_new_started: the destination may hold an unverified new copy.
    # muu_keep_stage: the stage may hold the only previous copy.
    muu_stage= muu_work= muu_have_lock=0 muu_keep_stage=0 muu_verified=0 muu_new_started=0 muu_new_identity=
    muu_lock=$muu_directory/.muu-install-lock
    trap 'muu_cleanup $?' 0
    trap 'exit 129' 1
    trap 'exit 130' 2
    trap 'exit 143' 15
    mkdir -p "$muu_directory" || muu_fail 'cannot create destination'
    # mkdir is atomic. A lock left by a crash is not removed automatically:
    # the script cannot tell it from another installer that is still running.
    if ! mkdir "$muu_lock"; then
        muu_report_stages
        muu_fail "another installer is active; inspect the lock before removing it: $muu_lock"
    fi
    muu_have_lock=1
    if [ "$muu_operation" = uninstall ]; then
        if ! muu_present "$muu_target"; then
            printf 'Muu IME のアプリは配置されていません。\n'
            return 0
        fi
        muu_stage=$(mktemp -d "$muu_directory/.muu-stage.XXXXXX") || muu_fail 'cannot create removal stage'
        mv "$muu_target" "$muu_stage/previous-MuuIME.app" || muu_fail 'cannot retire installed app'
        muu_verified=1
        muu_retire_previous
        printf 'アプリを取り外しました。辞書・学習・モデル・設定は変更していません。システム設定でMuu IMEの入力ソースを外し、ログアウトしてログインし直してください。\n'
        return 0
    fi
    muu_work=$(mktemp -d "${TMPDIR:-/tmp}/muu-install.XXXXXX") || muu_fail 'cannot create download directory'
    muu_work=$(cd "$muu_work" && pwd -P) || muu_fail 'cannot resolve download directory'
    printf 'GitHub Releases と SHA-256 を照合します。ad-hoc 署名で、Developer ID 署名・公証はありません。\n'
    # The version comes from SHA256SUMS, and the ZIP is then fetched from that
    # version's tag, never from latest/ again. If a release is published while
    # this runs, the checksum and the ZIP still belong to one version.
    if [ -n "$muu_version_arg" ]; then
        muu_manifest_url=$muu_base/download/v$muu_version_arg/SHA256SUMS
    else
        muu_manifest_url=$muu_base/latest/download/SHA256SUMS
    fi
    muu_fetch "$muu_manifest_url" "$muu_work/SHA256SUMS.partial" 4096 || muu_fail 'checksum download failed'
    [ "$(wc -c < "$muu_work/SHA256SUMS.partial")" -le 4096 ] || muu_fail 'checksum file is too large'
    # The manifest is data, never shell syntax. Require its terminating newline
    # as well as exactly one SHA, two spaces and the fixed release filename.
    [ "$(wc -l < "$muu_work/SHA256SUMS.partial" | tr -d ' ')" = 1 ] || muu_fail 'invalid checksum line count'
    awk 'NR != 1 || $0 !~ /^[0-9a-f]{64}  MuuIME-[0-9]+\.[0-9]+\.[0-9]+-macos-arm64[.]zip$/ {exit 1}
        END {if (NR != 1) exit 1}' "$muu_work/SHA256SUMS.partial" || muu_fail 'invalid checksum format'
    muu_manifest=$(cat "$muu_work/SHA256SUMS.partial") || muu_fail 'cannot read checksum'
    muu_digest=${muu_manifest%%  *}
    muu_archive=${muu_manifest#*  }
    muu_selected_version=${muu_archive#MuuIME-}
    muu_selected_version=${muu_selected_version%-macos-arm64.zip}
    [ -z "$muu_version_arg" ] || [ "$muu_selected_version" = "$muu_version_arg" ] || muu_fail 'checksum version mismatch'
    muu_fetch "$muu_base/download/v$muu_selected_version/$muu_archive" "$muu_work/archive.partial" "$muu_zip_limit" || muu_fail 'ZIP download failed'
    # Order matters: hash first, then the entry list and archive test, and
    # only then extract. Nothing from an unverified ZIP reaches the disk.
    muu_actual_digest=$(shasum -a 256 "$muu_work/archive.partial" | awk '{print $1}') || muu_fail 'cannot hash ZIP'
    [ "$muu_actual_digest" = "$muu_digest" ] || muu_fail 'ZIP checksum mismatch'
    muu_archive_safe "$muu_work/archive.partial" || muu_fail 'unsafe ZIP entries'
    mkdir "$muu_work/extracted" || muu_fail 'cannot prepare extraction'
    ditto -x -k --norsrc --noqtn "$muu_work/archive.partial" "$muu_work/extracted" || muu_fail 'ZIP extraction failed'
    [ "$(find "$muu_work/extracted" -mindepth 1 -maxdepth 1 | wc -l | tr -d ' ')" = 1 ] || muu_fail 'unexpected ZIP root'
    muu_source=$muu_work/extracted/MuuIME.app
    muu_expanded_safe "$muu_source" || muu_fail 'expanded app size limit exceeded'
    muu_verify_app "$muu_source" source || muu_fail 'downloaded app failed verification'
    # The stage sits beside the destination, so the backup and replacement
    # below are renames on one volume, not copies.
    muu_stage=$(mktemp -d "$muu_directory/.muu-stage.XXXXXX") || muu_fail 'cannot create installation stage'
    ditto --norsrc --noqtn "$muu_source" "$muu_stage/new-MuuIME.app" || muu_fail 'stage copy failed'
    muu_same_app "$muu_stage/new-MuuIME.app" stage || muu_fail 'staged app failed verification'
    muu_new_identity=$(stat -f '%d:%i' "$muu_stage/new-MuuIME.app") || muu_fail 'cannot identify staged app'
    muu_no_links "$muu_target" || muu_fail 'destination path changed'
    if muu_present "$muu_target"; then
        mv "$muu_target" "$muu_stage/previous-MuuIME.app" || muu_fail 'cannot back up existing app'
    fi
    muu_present "$muu_target" && muu_fail 'destination must be absent before rename'
    muu_new_started=1
    mv "$muu_stage/new-MuuIME.app" "$muu_target" || muu_fail 'cannot place new app'
    # Check after the rename and again after verification, so the app that
    # was verified is the inode that now sits at the destination.
    muu_matches_identity "$muu_target" "$muu_new_identity" || muu_fail "destination changed during placement: $muu_target"
    muu_same_app "$muu_target" installed || muu_fail 'installed app failed verification'
    muu_matches_identity "$muu_target" "$muu_new_identity" || muu_fail "destination changed during verification: $muu_target"
    muu_verified=1
    muu_retire_previous
    printf 'Muu IME %s を配置しました。ログアウトしてログインし直し、システム設定の入力ソースで「Muu IME ひらがな」と「Muu IME 英数」を追加してください。\n' "$muu_selected_version"
}

muu_main "$@"
