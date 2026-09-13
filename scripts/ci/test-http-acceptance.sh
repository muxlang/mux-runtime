#!/usr/bin/env bash
# Test configuration and protocol probes without an external endpoint.
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
output_dir="$(mktemp -d "${TMPDIR:-/tmp}/mux-http-acceptance-test.XXXXXX")"
bin_dir="$output_dir/bin"
curl_log="$output_dir/curl-args"
mkdir -p "$bin_dir"
trap 'rm -rf -- "$output_dir"' EXIT

cat >"$bin_dir/curl" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

printf '%s\n' "$@" >>"${MUX_FAKE_CURL_LOG:?}"
if [[ "${1:-}" == '--version' ]]; then
    printf '%s\n' \
        'curl 8.0.0' \
        'Release-Date: 2023-02-20' \
        'Protocols: http https' \
        'Features: HTTP2 HTTP3'
    exit 0
fi

output=''
protocol=''
while (($# > 0)); do
    case "$1" in
        --output)
            output="$2"
            shift 2
            ;;
        --write-out)
            shift 2
            ;;
        --http1.1|--http2|--http3-only)
            protocol="$1"
            shift
            ;;
        --header|--max-time|--connect-timeout|--max-filesize|--proto|--proto-redir)
            shift 2
            ;;
        --silent|--show-error|--fail-with-body|--location)
            shift
            ;;
        http://*|https://*)
            shift
            ;;
        *)
            printf 'unexpected curl argument: %s\n' "$1" >&2
            exit 1
            ;;
    esac
done

if [[ -z "$output" || "$output" == '--write-out' || -z "$protocol" ]]; then
    echo 'fake curl received malformed probe arguments' >&2
    exit 1
fi
case "$protocol" in
    --http1.1) version='1.1' ;;
    --http2) version='2' ;;
    --http3-only) version='3' ;;
    *) exit 1 ;;
esac
printf 'fake response\n' >"$output"
printf '200\t%s\n' "$version"
EOF
chmod +x "$bin_dir/curl"

set +e
PATH="$bin_dir:$PATH" MUX_HTTP1_URL='' MUX_HTTP2_URL='' MUX_HTTP3_URL='' \
    "$script_dir/run-http-acceptance.sh" >"$output_dir/stdout" 2>"$output_dir/stderr"
status=$?
set -e

if [[ "$status" -ne 2 ]]; then
    echo "HTTP acceptance returned $status without endpoint secrets" >&2
    cat "$output_dir/stderr" >&2
    exit 1
fi
grep -Fq 'MUX_HTTP1_URL' "$output_dir/stderr"
grep -Fq 'MUX_HTTP2_URL' "$output_dir/stderr"
grep -Fq 'MUX_HTTP3_URL' "$output_dir/stderr"
if grep -Fq 'HTTP/1.1 accepted' "$output_dir/stdout" || \
    grep -Fq 'HTTP/2 accepted' "$output_dir/stdout"; then
    echo 'HTTP acceptance contacted an endpoint during the missing-secret test' >&2
    exit 1
fi

fixture_token_prefix='mux-ci-'
http1_token="${fixture_token_prefix}one"
http2_token="${fixture_token_prefix}two"
http3_token="${fixture_token_prefix}three"

set +e
MUX_FAKE_CURL_LOG="$curl_log" PATH="$bin_dir:$PATH" \
    MUX_HTTP1_URL='http://http1.example.test/probe' \
    MUX_HTTP2_URL='https://http2.example.test/probe' \
    MUX_HTTP3_URL='https://http3.example.test/probe' \
    MUX_HTTP1_BEARER_TOKEN="$http1_token" \
    MUX_HTTP2_BEARER_TOKEN="$http2_token" \
    MUX_HTTP3_BEARER_TOKEN="$http3_token" \
    "$script_dir/run-http-acceptance.sh" >"$output_dir/success-stdout" \
    2>"$output_dir/success-stderr"
success_status=$?
set -e
if [[ "$success_status" -ne 0 ]]; then
    echo "HTTP acceptance failed in fake-curl success test" >&2
    cat "$output_dir/success-stdout" >&2
    cat "$output_dir/success-stderr" >&2
    cat "$curl_log" >&2
    exit 1
fi
grep -Fq 'HTTP/1.1 accepted: HTTP/1.1' "$output_dir/success-stdout"
grep -Fq 'HTTP/2 accepted: HTTP/2' "$output_dir/success-stdout"
grep -Fq 'HTTP/3 accepted: HTTP/3' "$output_dir/success-stdout"
grep -Fq -- '--http1.1' "$curl_log"
grep -Fq -- '--http2' "$curl_log"
grep -Fq -- '--http3-only' "$curl_log"
grep -Fq -- "Authorization: Bearer $http1_token" "$curl_log"
grep -Fq -- "Authorization: Bearer $http2_token" "$curl_log"
grep -Fq -- "Authorization: Bearer $http3_token" "$curl_log"

for limit_variable in MUX_HTTP_ACCEPTANCE_TIMEOUT_SECS MUX_HTTP_ACCEPTANCE_MAX_BODY_BYTES; do
    set +e
    env "$limit_variable=18446744073709551617" \
        MUX_FAKE_CURL_LOG="$curl_log" PATH="$bin_dir:$PATH" \
        MUX_HTTP1_URL='http://http1.example.test/probe' \
        MUX_HTTP2_URL='https://http2.example.test/probe' \
        MUX_HTTP3_URL='https://http3.example.test/probe' \
        "$script_dir/run-http-acceptance.sh" >"$output_dir/limit-stdout" \
        2>"$output_dir/limit-stderr"
    status=$?
    set -e
    if [[ "$status" -ne 2 ]]; then
        echo "HTTP acceptance did not reject an overflowing $limit_variable" >&2
        exit 1
    fi
done

echo 'HTTP acceptance configuration test passed'
