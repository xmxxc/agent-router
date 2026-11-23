#!/bin/bash
# Record the library from the actual curl-sys build, separately from system curl.
set -euo pipefail
binary=target/$TARGET/release/wfc
sha256sum --check --strict "$EVIDENCE/binary.sha256"
readelf -hW -lW -dW -VW "$binary" | tee "$EVIDENCE/elf.log"
ldd "$binary" | tee "$EVIDENCE/ldd.log"
if grep -Fq 'not found' "$EVIDENCE/ldd.log" ||
   grep -Eq 'Shared library: \[libcurl\.so' "$EVIDENCE/elf.log"; then
    printf 'Missing runtime library or unexpected shared libcurl\n' >&2
    exit 1
fi
{
    getconf GNU_LIBC_VERSION
    printf 'Versioned GLIBC symbols required by wfc\n'
    required=$(grep -Eo 'GLIBC_[0-9]+(\.[0-9]+)+' "$EVIDENCE/elf.log" | sort -Vu)
    printf '%s\n' "$required"
    printf 'Max required version: %s\n' "$(tail -n 1 <<< "$required")"
} | tee "$EVIDENCE/glibc.log"

cargo tree --locked --target "$TARGET" -p wfc -e features -i curl-sys |
    tee "$EVIDENCE/libraries.log"
archives=(target/"$TARGET"/release/build/curl-sys-*/out/build/libcurl.a)
test "${#archives[@]}" -eq 1
archive=${archives[0]}
test -f "$archive"
include=$(dirname "$(dirname "$archive")")/include
probe="$RUNNER_TEMP/wfc-opencode-curl-version"
# The flags from pkg-config are separate argv items for the system linker.
flags=$(pkg-config --libs openssl zlib)
read -r -a libraries <<< "$flags"
{
    sha256sum "$archive"
    cc -I "$include" -x c - -x none "$archive" "${libraries[@]}" -pthread -ldl -lm -o "$probe" <<'C'
#include <stdio.h>
#include <curl/curl.h>

int main(void) {
    if (curl_global_init(CURL_GLOBAL_DEFAULT) != CURLE_OK)
        return 1;
    curl_version_info_data *version = curl_version_info(CURLVERSION_NOW);
    printf("Bundled libcurl %s\n", version->version);
    printf("Bundled libcurl TLS backend %s\n",
           version->ssl_version ? version->ssl_version : "unavailable");
    printf("Bundled libcurl zlib %s\n",
           version->libz_version ? version->libz_version : "unavailable");
    int missing_ssl = version->ssl_version == NULL;
    curl_global_cleanup();
    return missing_ssl;
}
C
    "$probe"
    ldd "$probe"
    openssl version -a
} 2>&1 | tee -a "$EVIDENCE/libraries.log"
