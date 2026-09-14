# Validate the record structure and coverage counters that the runtime gates
# consume, then print aggregate metrics as four whitespace-separated integers.
# LCOV has many optional record fields; this parser deliberately validates the
# required line/branch counters without rejecting fields added by newer tools.

function fail(message) {
    if (!invalid) {
        printf "invalid LCOV at line %d: %s\n", NR, message > "/dev/stderr"
    }
    invalid = 1
}

function require_record(tag) {
    if (!in_record) {
        fail(tag " appears outside a source record")
        return 0
    }
    return 1
}

function count_value(tag, value,   parsed) {
    if (value !~ /^[0-9]+$/) {
        fail(tag " must contain a non-negative decimal integer")
        return 0
    }
    parsed = value + 0
    return parsed
}

function finish_record(   message) {
    if (!in_record) {
        fail("end_of_record appears without a source record")
        return
    }
    record_count++
    if (!has_lf || !has_lh) {
        fail("each source record must contain both LF and LH")
    }
    if (has_lf && has_lh && line_hit > line_found) {
        fail("LH cannot exceed LF")
    }
    if (has_brf != has_brh) {
        fail("BRF and BRH must appear together")
    }
    if (has_brf && has_brh && branch_hit > branch_found) {
        fail("BRH cannot exceed BRF")
    }
    total_lines += line_found
    covered_lines += line_hit
    total_branches += branch_found
    covered_branches += branch_hit
    in_record = 0
    has_lf = 0
    has_lh = 0
    has_brf = 0
    has_brh = 0
    line_found = 0
    line_hit = 0
    branch_found = 0
    branch_hit = 0
}

{
    sub(/\r$/, "")

    if ($0 == "end_of_record") {
        finish_record()
        next
    }

    if ($0 ~ /^TN:/) {
        if (in_record) {
            fail("TN appears before the previous source record ended")
        }
        next
    }

    if ($0 ~ /^SF:/) {
        if (in_record) {
            fail("SF appears before the previous source record ended")
        }
        in_record = 1
        next
    }

    if ($0 ~ /^LF:/) {
        if (require_record("LF") && has_lf) {
            fail("duplicate LF in source record")
        }
        if (require_record("LF")) {
            line_found = count_value("LF", substr($0, 4))
            has_lf = 1
        }
        next
    }

    if ($0 ~ /^LH:/) {
        if (require_record("LH") && has_lh) {
            fail("duplicate LH in source record")
        }
        if (require_record("LH")) {
            line_hit = count_value("LH", substr($0, 4))
            has_lh = 1
        }
        next
    }

    if ($0 ~ /^BRF:/) {
        if (require_record("BRF") && has_brf) {
            fail("duplicate BRF in source record")
        }
        if (require_record("BRF")) {
            branch_found = count_value("BRF", substr($0, 5))
            has_brf = 1
        }
        next
    }

    if ($0 ~ /^BRH:/) {
        if (require_record("BRH") && has_brh) {
            fail("duplicate BRH in source record")
        }
        if (require_record("BRH")) {
            branch_hit = count_value("BRH", substr($0, 5))
            has_brh = 1
        }
        next
    }

    # Other standard LCOV fields are optional and intentionally ignored. A
    # malformed required field still fails because its corresponding counter
    # is absent or has an invalid value.
    if (!in_record && $0 != "") {
        fail("record data appears before SF")
    }
}

END {
    if (in_record) {
        fail("source record is missing end_of_record")
    }
    if (record_count == 0) {
        fail("report contains no complete source records")
    }
    if (invalid) {
        exit 1
    }
    printf "%d %d %d %d\n", total_lines, covered_lines, total_branches, covered_branches
}
