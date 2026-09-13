#!/usr/bin/env bash
# Probe configured HTTP/1.1, HTTP/2, and HTTP/3 endpoints with curl.
set -euo pipefail

readonly default_timeout_seconds=20
readonly max_timeout_seconds=60
readonly default_max_body_bytes=1048576
readonly max_max_body_bytes=16777216

required=(MUX_HTTP1_URL MUX_HTTP2_URL MUX_HTTP3_URL)
missing=()
for variable in "${required[@]}"; do
    if [[ -z "${!variable:-}" ]]; then
        missing+=("$variable")
    fi
done

if (( ${#missing[@]} > 0 )); then
    printf 'HTTP protocol acceptance is not configured. Missing secrets: %s\n' \
        "${missing[*]}" >&2
    printf 'Configure all three secrets, then dispatch the Hosted HTTP Acceptance workflow again.\n' >&2
    exit 2
fi

if ! command -v curl >/dev/null 2>&1; then
    echo 'HTTP protocol acceptance requires curl on PATH.' >&2
    exit 1
fi

if ! curl --version | awk '/^Features:/ { for (i = 2; i <= NF; i++) if ($i == "HTTP2") found = 1 } END { exit(found ? 0 : 1) }'; then
    echo 'HTTP protocol acceptance requires a curl build with HTTP/2 support.' >&2
    exit 1
fi
if ! curl --version | awk '/^Features:/ { for (i = 2; i <= NF; i++) if ($i == "HTTP3") found = 1 } END { exit(found ? 0 : 1) }'; then
    echo 'HTTP protocol acceptance requires a curl build with HTTP/3 support.' >&2
    exit 1
fi

timeout_seconds="${MUX_HTTP_ACCEPTANCE_TIMEOUT_SECS:-$default_timeout_seconds}"
max_body_bytes="${MUX_HTTP_ACCEPTANCE_MAX_BODY_BYTES:-$default_max_body_bytes}"

within_positive_limit() {
    local value="$1"
    local maximum="$2"
    [[ "$value" =~ ^[1-9][0-9]*$ ]] &&
        (( ${#value} <= ${#maximum} )) &&
        (( value <= maximum ))
}

if ! within_positive_limit "$timeout_seconds" "$max_timeout_seconds"; then
    printf 'MUX_HTTP_ACCEPTANCE_TIMEOUT_SECS must be an integer from 1 to %d.\n' \
        "$max_timeout_seconds" >&2
    exit 2
fi
if ! within_positive_limit "$max_body_bytes" "$max_max_body_bytes"; then
    printf 'MUX_HTTP_ACCEPTANCE_MAX_BODY_BYTES must be an integer from 1 to %d.\n' \
        "$max_max_body_bytes" >&2
    exit 2
fi

validate_url() {
    local label="$1"
    local url="$2"
    local required_scheme="$3"

    if [[ "$url" =~ [[:space:]] ]]; then
        printf '%s endpoint URL contains whitespace.\n' "$label" >&2
        return 1
    fi
    case "$url" in
        http://*|https://*)
            ;;
        *)
            printf '%s endpoint URL must start with http:// or https://.\n' "$label" >&2
            return 1
            ;;
    esac
    if [[ "$required_scheme" == https && "$url" != https://* ]]; then
        printf '%s endpoint must use HTTPS.\n' \
            "$label" >&2
        return 1
    fi
}

run_probe() {
    local label="$1"
    local url="$2"
    local expected_version="$3"
    local token_variable="$4"
    local protocol_flag="$5"
    local body_file
    local metadata_file
    local error_file
    local curl_status
    local response_status
    local response_version
    local body_size
    local token="${!token_variable:-}"
    body_file="$(mktemp "${TMPDIR:-/tmp}/mux-http-body.XXXXXX")"
    metadata_file="$(mktemp "${TMPDIR:-/tmp}/mux-http-metadata.XXXXXX")"
    error_file="$(mktemp "${TMPDIR:-/tmp}/mux-http-error.XXXXXX")"
    trap 'rm -f -- "$body_file" "$metadata_file" "$error_file"' RETURN

    local -a curl_args=(
        --silent
        --show-error
        --fail-with-body
        --location
        --max-time "$timeout_seconds"
        --connect-timeout "$timeout_seconds"
        --max-filesize "$max_body_bytes"
        --proto '=http,https'
        --proto-redir '=https'
        "$protocol_flag"
        --output "$body_file"
        --write-out '%{http_code}\t%{http_version}\n'
    )

    if [[ -n "$token" ]]; then
        curl_args+=(--header "Authorization: Bearer $token")
    fi

    if curl "${curl_args[@]}" "$url" >"$metadata_file" 2>"$error_file"; then
        curl_status=0
    else
        curl_status=$?
    fi
    if (( curl_status != 0 )); then
        printf '%s acceptance request failed with curl status %d.\n' "$label" "$curl_status" >&2
        if [[ -s "$error_file" ]]; then
            sed -E 's#https?://[^[:space:]]+#<redacted-url>#g' "$error_file" \
                | tr '\n' ' ' >&2
            printf '\n' >&2
        fi
        return 1
    fi

    IFS=$'\t' read -r response_status response_version < "$metadata_file"
    if [[ ! "$response_status" =~ ^2[0-9][0-9]$ ]]; then
        printf '%s endpoint returned HTTP status %s; expected a 2xx response.\n' \
            "$label" "${response_status:-unknown}" >&2
        return 1
    fi
    if [[ "$response_version" != "$expected_version" ]]; then
        printf '%s endpoint negotiated HTTP/%s; expected HTTP/%s.\n' \
            "$label" "${response_version:-unknown}" "$expected_version" >&2
        return 1
    fi

    body_size="$(wc -c < "$body_file")"
    if (( body_size > max_body_bytes )); then
        printf '%s endpoint returned %d body bytes; the limit is %d.\n' \
            "$label" "$body_size" "$max_body_bytes" >&2
        return 1
    fi

    printf '%s accepted: HTTP/%s, status %s, body %d bytes\n' \
        "$label" "$response_version" "$response_status" "$body_size"
}

validate_url 'HTTP/1.1' "$MUX_HTTP1_URL" http
validate_url 'HTTP/2' "$MUX_HTTP2_URL" https
validate_url 'HTTP/3' "$MUX_HTTP3_URL" https
run_probe 'HTTP/1.1' "$MUX_HTTP1_URL" 1.1 MUX_HTTP1_BEARER_TOKEN --http1.1
run_probe 'HTTP/2' "$MUX_HTTP2_URL" 2 MUX_HTTP2_BEARER_TOKEN --http2
run_probe 'HTTP/3' "$MUX_HTTP3_URL" 3 MUX_HTTP3_BEARER_TOKEN --http3-only
echo 'HTTP protocol acceptance passed'
