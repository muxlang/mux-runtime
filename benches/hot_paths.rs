// Criterion benchmarks for the runtime's hot paths, driven through the same
// `extern "C"` entry points that compiled Mux code calls.
//
// Ownership contract (verified against the runtime source, see AGENTS.md):
//   * Value constructors (`mux_box_int`, `mux_optional_some_int`,
//     `mux_result_ok_int`, `mux_new_string_from_cstr`, ...) return an owned
//     `*mut Value` released with `mux_rc_dec`.
//   * Container mutators/wrappers (`mux_list_push`, `mux_map_put`,
//     `mux_set_add`, `mux_new_tuple`) CLONE their value arguments, so the caller
//     still owns and frees what it passed in.
//   * `*_value` wrappers (`mux_list_value`, `mux_map_value`, `mux_set_value`,
//     `mux_tuple_value`) CONSUME the raw container pointer and return an owned
//     `*mut Value` to free with `mux_rc_dec`.
//   * Raw containers built for read fixtures are freed with `mux_free_list` /
//     `mux_free_map` / `mux_free_set`; owned C strings with `mux_free_string`.
//   * Reads that return a fresh Value (`mux_list_get`, `mux_map_get`) are owned
//     and freed with `mux_rc_dec`.
// Every benchmark frees what it allocates, so repeated iterations do not leak.
//
// Module coverage (why each src/ module is or is not micro-benched here):
//   refcount, boxing ....... benched (alloc/inc/dec/clone)
//   int, float, math ....... benched via the `primitive` group
//   list, map, set ......... benched: build + read (get/contains) + combine
//   string ................. benched (alloc/concat/length/equal)
//   optional, result, tuple  benched (construct/query)
//   json ................... benched (parse + stringify)
//   bool, data ............. trivial conversions; folded into `primitive`
//   datetime, random ....... nondeterministic wall-clock/entropy; low signal
//   io, panic, assert ...... side effects / control flow, not perf hot paths
//   closure ................ heap layout is built by codegen-emitted code; no
//                            standalone constructor to bench from Rust
//   object ................. needs codegen-constructed instances + vtables
//   net, sql, sync ......... require live services / threads; not a micro-bench
//
// Local/manual + non-blocking CI report; not a merge gate.

use std::ffi::CString;
use std::ptr;
use std::time::Duration;

use criterion::{black_box, criterion_group, criterion_main, Criterion};

use mux_runtime::boxing::mux_box_int;
use mux_runtime::float::mux_float_mul;
use mux_runtime::int::{mux_int_add, mux_int_to_string};
use mux_runtime::json::{mux_json_parse, mux_json_stringify};
use mux_runtime::list::{
    mux_list_concat, mux_list_contains, mux_list_get, mux_list_push, mux_list_to_string, List,
};
use mux_runtime::map::{mux_map_contains, mux_map_get, mux_map_put, mux_map_value, Map};
use mux_runtime::math::mux_int_pow;
use mux_runtime::optional::{mux_optional_is_some, mux_optional_some_int};
use mux_runtime::refcount::{mux_rc_clone, mux_rc_dec, mux_rc_inc};
use mux_runtime::result::{mux_result_is_ok, mux_result_ok_int};
use mux_runtime::set::{mux_set_add, mux_set_contains, mux_set_union, mux_set_value, Set};
use mux_runtime::std::{
    mux_free_list, mux_free_map, mux_free_set, mux_free_string, mux_list_value, mux_new_list,
    mux_new_map, mux_new_set,
};
use mux_runtime::string::{
    mux_new_string_from_cstr, mux_string_concat, mux_string_equal, mux_string_length,
};
use mux_runtime::tuple::{mux_new_tuple, mux_tuple_value};

// Fixture size for build/read benchmarks: large enough to exercise growth and
// hashing, small enough to keep an iteration cheap.
const N: i64 = 256;

fn build_list(n: i64) -> *mut List {
    let list = mux_new_list();
    for i in 0..n {
        let e = mux_box_int(i);
        mux_list_push(list, e);
        mux_rc_dec(e);
    }
    list
}

fn build_map(n: i64) -> *mut Map {
    let map = mux_new_map();
    for i in 0..n {
        let k = mux_box_int(i);
        let v = mux_box_int(i * 2);
        mux_map_put(map, k, v);
        mux_rc_dec(k);
        mux_rc_dec(v);
    }
    map
}

fn build_set(n: i64) -> *mut Set {
    let set = mux_new_set();
    for i in 0..n {
        let e = mux_box_int(i);
        mux_set_add(set, e);
        mux_rc_dec(e);
    }
    set
}

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
            let cloned = mux_rc_clone(black_box(held));
            mux_rc_dec(cloned);
        });
    });
    mux_rc_dec(held);
    group.finish();
}

fn bench_primitive(c: &mut Criterion) {
    let mut group = c.benchmark_group("primitive");
    group.bench_function("int_add", |b| {
        b.iter(|| mux_int_add(black_box(21), black_box(21)));
    });
    group.bench_function("int_pow", |b| {
        b.iter(|| mux_int_pow(black_box(2), black_box(20)));
    });
    group.bench_function("float_mul", |b| {
        b.iter(|| mux_float_mul(black_box(1.5), black_box(2.5)));
    });
    group.bench_function("int_to_string", |b| {
        b.iter(|| {
            let s = mux_int_to_string(black_box(1_234_567_890));
            mux_free_string(s);
        });
    });
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

    let list = build_list(N);
    let list2 = build_list(N);
    let needle = mux_box_int(N / 2);
    group.bench_function("get", |b| {
        b.iter(|| {
            let v = mux_list_get(list, black_box(N / 2));
            mux_rc_dec(v);
        });
    });
    group.bench_function("contains", |b| {
        b.iter(|| black_box(mux_list_contains(list, needle)));
    });
    group.bench_function("concat", |b| {
        b.iter(|| {
            let joined = mux_list_concat(list, list2);
            mux_free_list(joined);
        });
    });
    group.bench_function("to_string", |b| {
        b.iter(|| {
            let s = mux_list_to_string(list);
            mux_free_string(s);
        });
    });
    group.finish();

    mux_rc_dec(needle);
    mux_free_list(list);
    mux_free_list(list2);
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

    let map = build_map(N);
    let key = mux_box_int(N / 2);
    group.bench_function("get", |b| {
        b.iter(|| {
            let v = mux_map_get(map, key);
            mux_rc_dec(v);
        });
    });
    group.bench_function("contains", |b| {
        b.iter(|| black_box(mux_map_contains(map, key)));
    });
    group.finish();

    mux_rc_dec(key);
    mux_free_map(map);
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

    let set = build_set(N);
    let set2 = build_set(N);
    let member = mux_box_int(N / 2);
    group.bench_function("contains", |b| {
        b.iter(|| black_box(mux_set_contains(set, member)));
    });
    group.bench_function("union", |b| {
        b.iter(|| {
            let u = mux_set_union(set, set2);
            mux_free_set(u);
        });
    });
    group.finish();

    mux_rc_dec(member);
    mux_free_set(set);
    mux_free_set(set2);
}

fn bench_string(c: &mut Criterion) {
    let mut group = c.benchmark_group("string");
    // Multi-byte UTF-8 content so length/alloc touch non-ASCII paths.
    let text = CString::new("the quick brown fox jumps: caffe latte").unwrap();
    let other = CString::new("the quick brown fox jumps: espresso doppio").unwrap();

    group.bench_function("alloc_free", |b| {
        b.iter(|| {
            let v = mux_new_string_from_cstr(black_box(text.as_ptr()));
            mux_rc_dec(v);
        });
    });
    group.bench_function("concat", |b| {
        b.iter(|| {
            let s = mux_string_concat(black_box(text.as_ptr()), black_box(other.as_ptr()));
            mux_free_string(s);
        });
    });
    group.bench_function("length", |b| {
        b.iter(|| black_box(mux_string_length(black_box(text.as_ptr()))));
    });
    group.bench_function("equal", |b| {
        b.iter(|| {
            black_box(mux_string_equal(
                black_box(text.as_ptr()),
                black_box(other.as_ptr()),
            ))
        });
    });
    group.finish();
}

fn bench_wrappers(c: &mut Criterion) {
    let mut group = c.benchmark_group("wrappers");
    group.bench_function("optional_some", |b| {
        b.iter(|| {
            let o = mux_optional_some_int(black_box(7));
            mux_rc_dec(o);
        });
    });
    group.bench_function("result_ok", |b| {
        b.iter(|| {
            let r = mux_result_ok_int(black_box(7));
            mux_rc_dec(r);
        });
    });
    group.bench_function("tuple_new", |b| {
        b.iter(|| {
            let l = mux_box_int(black_box(1));
            let r = mux_box_int(black_box(2));
            let t = mux_new_tuple(l, r);
            mux_rc_dec(l);
            mux_rc_dec(r);
            mux_rc_dec(mux_tuple_value(t));
        });
    });

    let some = mux_optional_some_int(7);
    let ok = mux_result_ok_int(7);
    group.bench_function("optional_is_some", |b| {
        b.iter(|| black_box(mux_optional_is_some(some)));
    });
    group.bench_function("result_is_ok", |b| {
        b.iter(|| black_box(mux_result_is_ok(ok)));
    });
    group.finish();

    mux_rc_dec(some);
    mux_rc_dec(ok);
}

fn bench_json(c: &mut Criterion) {
    let mut group = c.benchmark_group("json");
    let small = CString::new(r#"{"a":1,"b":[1,2,3],"c":"hello"}"#).unwrap();
    let medium = CString::new(concat!(
        r#"{"users":[{"id":1,"name":"alice","tags":["a","b"]},"#,
        r#"{"id":2,"name":"bob","tags":[]}],"#,
        r#""meta":{"count":2,"ok":true,"ratio":0.25}}"#
    ))
    .unwrap();

    // `mux_json_parse` returns a `Value::Result`; assert Ok so a malformed
    // payload fails the bench instead of silently timing the error path.
    group.bench_function("parse_small", |b| {
        b.iter(|| {
            let v = mux_json_parse(black_box(small.as_ptr()));
            assert!(mux_result_is_ok(v), "benchmark JSON payload should parse");
            mux_rc_dec(v);
        });
    });
    group.bench_function("parse_medium", |b| {
        b.iter(|| {
            let v = mux_json_parse(black_box(medium.as_ptr()));
            assert!(mux_result_is_ok(v), "benchmark JSON payload should parse");
            mux_rc_dec(v);
        });
    });

    // Serialize a real 256-element list value (not a Result wrapper) so the
    // stringify path does meaningful work.
    let doc = mux_list_value(build_list(N));
    group.bench_function("stringify", |b| {
        b.iter(|| {
            let s = mux_json_stringify(doc, ptr::null_mut());
            mux_rc_dec(s);
        });
    });
    group.finish();

    mux_rc_dec(doc);
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
    targets = bench_refcount,
        bench_primitive,
        bench_list,
        bench_map,
        bench_set,
        bench_string,
        bench_wrappers,
        bench_json
);
criterion_main!(benches);
