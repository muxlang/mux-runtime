//! Unit tests for the core `Value` type: Display, ordering, equality, hashing,
//! type tags, and conversions.
#![allow(clippy::mutable_key_type)]

use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};

use mux_runtime::{Tuple, Value};
use ordered_float::OrderedFloat;

fn hash_of(v: &Value) -> u64 {
    let mut h = DefaultHasher::new();
    v.hash(&mut h);
    h.finish()
}

#[test]
fn value_display_scalars() {
    assert_eq!(format!("{}", Value::Unit), "()");
    assert_eq!(format!("{}", Value::Bool(true)), "true");
    assert_eq!(format!("{}", Value::Int(5)), "5");
    assert_eq!(format!("{}", Value::Float(OrderedFloat(2.5))), "2.5");
    assert_eq!(format!("{}", Value::String("hi".to_string())), "hi");
    assert_eq!(format!("{}", Value::Opaque(vec![0u8, 1, 2].into_boxed_slice())), "<Opaque 3 bytes>");
}

#[test]
fn value_display_composites() {
    assert_eq!(format!("{}", Value::List(vec![Value::Int(1), Value::Int(2)])), "[1, 2]");

    let mut map = BTreeMap::new();
    map.insert(Value::Int(1), Value::Int(2));
    assert_eq!(format!("{}", Value::Map(map)), "{1: 2}");

    let mut set = BTreeSet::new();
    set.insert(Value::Int(1));
    set.insert(Value::Int(2));
    assert_eq!(format!("{}", Value::Set(set)), "{1, 2}");

    assert_eq!(
        format!("{}", Value::Tuple(Box::new(Tuple(Value::Int(1), Value::Int(2))))),
        "(1, 2)"
    );
    assert_eq!(format!("{}", Value::Optional(Some(Box::new(Value::Int(1))))), "Some(1)");
    assert_eq!(format!("{}", Value::Optional(None)), "None");
    assert_eq!(format!("{}", Value::Result(Ok(Box::new(Value::Int(1))))), "Ok(1)");
    assert_eq!(format!("{}", Value::Result(Err(Box::new(Value::String("e".to_string()))))), "Err(e)");
}

#[test]
fn value_type_tags() {
    assert_eq!(Value::Bool(false).type_tag(), 0);
    assert_eq!(Value::Int(0).type_tag(), 1);
    assert_eq!(Value::Float(OrderedFloat(0.0)).type_tag(), 2);
    assert_eq!(Value::String(String::new()).type_tag(), 3);
    assert_eq!(Value::List(Vec::new()).type_tag(), 4);
    assert_eq!(Value::Unit.type_tag(), 11);
}

#[test]
fn value_equality_and_ordering() {
    assert_eq!(Value::Int(1), Value::Int(1));
    assert_ne!(Value::Int(1), Value::Int(2));
    assert_ne!(Value::Int(1), Value::Bool(true));
    assert!(Value::Int(1) < Value::Int(2));
    // Cross-variant ordering follows variant order: Bool before Int.
    assert!(Value::Bool(true) < Value::Int(0));
}

#[test]
fn value_hash_consistent() {
    assert_eq!(hash_of(&Value::Int(7)), hash_of(&Value::Int(7)));
    assert_ne!(hash_of(&Value::Int(7)), hash_of(&Value::Int(8)));
}

#[test]
fn value_from_string() {
    assert_eq!(Value::from("abc".to_string()), Value::String("abc".to_string()));
}

#[test]
fn value_ordering_across_many_variants() {
    // variant order: Unit < Bool < Int < Float < String < List < Map < Set < ...
    let ordered = [
        Value::Unit,
        Value::Bool(false),
        Value::Int(0),
        Value::Float(OrderedFloat(0.0)),
        Value::String(String::new()),
        Value::List(vec![]),
    ];
    for pair in ordered.windows(2) {
        assert!(pair[0] < pair[1], "{:?} should be < {:?}", pair[0], pair[1]);
    }
    // same-variant comparisons
    assert!(Value::String("a".into()) < Value::String("b".into()));
    assert!(Value::List(vec![Value::Int(1)]) < Value::List(vec![Value::Int(2)]));
}

#[test]
fn value_equality_same_variant_data() {
    assert_eq!(
        Value::List(vec![Value::Int(1)]),
        Value::List(vec![Value::Int(1)])
    );
    assert_ne!(Value::Unit, Value::Bool(false));
    assert_eq!(Value::Unit, Value::Unit);
    let bytes = Value::Opaque(vec![1u8, 2].into_boxed_slice());
    assert_eq!(bytes.clone(), bytes);
}

#[test]
fn tuple_hash_and_compare() {
    let t1 = Value::Tuple(Box::new(Tuple(Value::Int(1), Value::Int(2))));
    let t2 = Value::Tuple(Box::new(Tuple(Value::Int(1), Value::Int(2))));
    assert_eq!(t1, t2);
    assert_eq!(hash_of(&t1), hash_of(&t2));
}

#[test]
fn object_value_display_debug_and_compare() {
    use mux_runtime::object::{mux_alloc_object, mux_free_object, mux_register_object_type};
    use std::ffi::CString;

    let name = CString::new("Widget").unwrap();
    let tid = mux_register_object_type(name.as_ptr(), 8);
    let obj = mux_alloc_object(tid);
    assert!(!obj.is_null());

    unsafe {
        let display = format!("{}", &*obj);
        assert!(display.contains("Object"), "got: {display}");
        // Debug formatting exercises ObjectData/ObjectRef Debug impls.
        let _ = format!("{:?}", &*obj);
        // An object compares equal to itself and hashes stably.
        assert_eq!(&*obj, &*obj);
        assert_eq!(hash_of(&*obj), hash_of(&*obj));
    }

    mux_free_object(obj);
}

#[test]
fn optional_and_result_hash() {
    let o1 = Value::Optional(Some(Box::new(Value::Int(1))));
    let o2 = Value::Optional(Some(Box::new(Value::Int(1))));
    assert_eq!(hash_of(&o1), hash_of(&o2));
    let r1 = Value::Result(Ok(Box::new(Value::Int(1))));
    let r2 = Value::Result(Err(Box::new(Value::Int(1))));
    assert_ne!(r1, r2);
}

#[test]
fn type_tags_for_all_variants() {
    let mut map = BTreeMap::new();
    map.insert(Value::Int(1), Value::Int(2));
    let mut set = BTreeSet::new();
    set.insert(Value::Int(1));
    assert_eq!(Value::Map(map).type_tag(), 5);
    assert_eq!(Value::Set(set).type_tag(), 6);
    assert_eq!(Value::Tuple(Box::new(Tuple(Value::Int(1), Value::Int(2)))).type_tag(), 10);
    assert_eq!(Value::Optional(None).type_tag(), 7);
    assert_eq!(Value::Result(Ok(Box::new(Value::Int(1)))).type_tag(), 8);
    assert_eq!(Value::Opaque(vec![0u8].into_boxed_slice()).type_tag(), 12);
}

#[test]
fn map_set_hash_and_order() {
    let mut m1 = BTreeMap::new();
    m1.insert(Value::String("a".into()), Value::Int(1));
    let mut m2 = BTreeMap::new();
    m2.insert(Value::String("a".into()), Value::Int(1));
    assert_eq!(hash_of(&Value::Map(m1.clone())), hash_of(&Value::Map(m2)));

    let mut s1 = BTreeSet::new();
    s1.insert(Value::Int(1));
    s1.insert(Value::Int(2));
    let s2 = s1.clone();
    assert_eq!(hash_of(&Value::Set(s1.clone())), hash_of(&Value::Set(s2)));

    // Optional / Result / Opaque ordering exercises those cmp arms.
    assert!(Value::Optional(None) < Value::Optional(Some(Box::new(Value::Int(0)))));
    assert!(
        Value::Result(Ok(Box::new(Value::Int(0))))
            < Value::Result(Err(Box::new(Value::Int(0))))
    );
    assert!(
        Value::Opaque(vec![1u8].into_boxed_slice()) < Value::Opaque(vec![2u8].into_boxed_slice())
    );
}

#[test]
fn empty_collection_display() {
    assert_eq!(format!("{}", Value::List(vec![])), "[]");
    assert_eq!(format!("{}", Value::Map(BTreeMap::new())), "{}");
    assert_eq!(format!("{}", Value::Set(BTreeSet::new())), "{}");
}
