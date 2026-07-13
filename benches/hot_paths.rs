// Criterion benchmarks for the runtime's hottest FFI surfaces, driven through
// the same `extern "C"` entry points that compiled Mux code calls.
//
// Ownership contract (verified against the runtime source):
//   * `mux_box_int` / `mux_new_string_from_cstr` return an owned `*mut Value`
//     that must be released with `mux_rc_dec`.
//   * `mux_list_push` / `mux_map_put` / `mux_set_add` CLONE their value
//     arguments, so the caller still owns (and must free) what it passed in.
//   * `mux_list_value` / `mux_map_value` / `mux_set_value` CONSUME the raw
//     collection pointer and return an owned `*mut Value` to free with
//     `mux_rc_dec`.
// Every benchmark frees what it allocates, so repeated iterations do not leak.
//
// Local/manual + non-blocking CI report; not a merge gate.

use std::ffi::CString;
use std::time::Duration;

use criterion::{black_box, criterion_group, criterion_main, Criterion};

use mux_runtime::boxing::mux_box_int;
use mux_runtime::json::Json;
use mux_runtime::map::{mux_map_put, mux_map_value};
use mux_runtime::refcount::{mux_rc_clone, mux_rc_dec, mux_rc_inc};
use mux_runtime::set::{mux_set_add, mux_set_value};
use mux_runtime::std::{mux_list_value, mux_new_list, mux_new_map, mux_new_set};
use mux_runtime::string::mux_new_string_from_cstr;

use mux_runtime::list::mux_list_push;

// Element count for the collection build/free benchmarks: large enough to
// exercise growth and hashing, small enough to keep an iteration cheap.
const N: i64 = 256;

fn bench_refcount(c: &mut Criterion) {
    let mut group = c.benchmark_group("refcount");

    group.bench_function("alloc_free", |b| {
        b.iter(|| {
            let v = mux_box_int(black_box(7));
            mux_rc_dec(v);
        });
    });

    // A persistent value whose count returns to 1 after each pair, so it stays
    // alive and is never freed inside the loop.
    let held = mux_box_int(7);
    group.bench_function("inc_dec", |b| {
        b.iter(|| {
            mux_rc_inc(black_box(held));
            mux_rc_dec(black_box(held));
        });
    });
    group.bench_function("clone", |b| {
        b.iter(|| {
            let c = mux_rc_clone(black_box(held));
            mux_rc_dec(c);
        });
    });
    mux_rc_dec(held);

    group.finish();
}

fn bench_list(c: &mut Criterion) {
    let mut group = c.benchmark_group("list");
    group.bench_function("build_free", |b| {
        b.iter(|| {
            let list = mux_new_list();
            for i in 0..N {
                let e = mux_box_int(i);
                mux_list_push(list, e);
                mux_rc_dec(e);
            }
            mux_rc_dec(mux_list_value(list));
        });
    });
    group.finish();
}

fn bench_map(c: &mut Criterion) {
    let mut group = c.benchmark_group("map");
    group.bench_function("build_free", |b| {
        b.iter(|| {
            let map = mux_new_map();
            for i in 0..N {
                let k = mux_box_int(i);
                let v = mux_box_int(i * 2);
                mux_map_put(map, k, v);
                mux_rc_dec(k);
                mux_rc_dec(v);
            }
            mux_rc_dec(mux_map_value(map));
        });
    });
    group.finish();
}

fn bench_set(c: &mut Criterion) {
    let mut group = c.benchmark_group("set");
    group.bench_function("build_free", |b| {
        b.iter(|| {
            let set = mux_new_set();
            for i in 0..N {
                let e = mux_box_int(i);
                mux_set_add(set, e);
                mux_rc_dec(e);
            }
            mux_rc_dec(mux_set_value(set));
        });
    });
    group.finish();
}

fn bench_string(c: &mut Criterion) {
    let mut group = c.benchmark_group("string");
    // Multi-byte UTF-8 content so length/allocation touch non-ASCII paths.
    let text = CString::new("the quick brown fox jumps: caffe latte, 2 + 2").unwrap();
    group.bench_function("alloc_free", |b| {
        b.iter(|| {
            let v = mux_new_string_from_cstr(black_box(text.as_ptr()));
            mux_rc_dec(v);
        });
    });
    group.finish();
}

fn bench_json(c: &mut Criterion) {
    let mut group = c.benchmark_group("json");
    let small = r#"{"a":1,"b":[1,2,3],"c":"hello"}"#;
    let medium = concat!(
        r#"{"users":[{"id":1,"name":"alice","tags":["a","b"]},"#,
        r#"{"id":2,"name":"bob","tags":[]}],"#,
        r#""meta":{"count":2,"ok":true,"ratio":0.25}}"#
    );
    for (name, payload) in [("small", small), ("medium", medium)] {
        group.bench_with_input(name, payload, |b, payload| {
            b.iter(|| {
                black_box(Json::parse(black_box(payload)).expect("benchmark JSON payload should parse"));
            });
        });
    }
    group.finish();
}

fn configured() -> Criterion {
    Criterion::default()
        .sample_size(50)
        .warm_up_time(Duration::from_millis(300))
        .measurement_time(Duration::from_millis(1500))
}

criterion_group!(
    name = benches;
    config = configured();
    targets = bench_refcount, bench_list, bench_map, bench_set, bench_string, bench_json
);
criterion_main!(benches);
