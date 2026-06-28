//! Unit tests for the collection cores (list, map, set).
#![allow(clippy::mutable_key_type)] // Value keys are logically immutable here.

use std::collections::{BTreeMap, BTreeSet};

use mux_runtime::list::List;
use mux_runtime::map::Map;
use mux_runtime::set::Set;
use mux_runtime::Value;

// --- List --------------------------------------------------------------------

#[test]
fn list_push_pop_length() {
    let mut list = List(Vec::new());
    assert_eq!(list.length(), 0);
    list.push_back(Value::Int(1));
    list.push_back(Value::Int(2));
    assert_eq!(list.length(), 2);
    assert_eq!(list.pop_back(), Some(Value::Int(2)));
    assert_eq!(list.length(), 1);
    assert_eq!(list.pop_back(), Some(Value::Int(1)));
    assert_eq!(list.pop_back(), None);
}

#[test]
fn list_display_contains_elements() {
    let list = List(vec![Value::Int(1), Value::Int(2)]);
    let s = format!("{}", list);
    assert!(s.contains('1') && s.contains('2'));
}

// --- Map ---------------------------------------------------------------------

#[test]
fn map_insert_get_remove_contains() {
    let mut map = Map(BTreeMap::new());
    map.insert(Value::String("k".to_string()), Value::Int(10));
    assert!(map.contains(&Value::String("k".to_string())));
    assert_eq!(
        map.get(&Value::String("k".to_string())),
        Some(&Value::Int(10))
    );
    assert_eq!(
        map.remove(&Value::String("k".to_string())),
        Some(Value::Int(10))
    );
    assert!(!map.contains(&Value::String("k".to_string())));
    assert_eq!(map.get(&Value::String("missing".to_string())), None);
}

// --- Set ---------------------------------------------------------------------

#[test]
fn set_add_remove_contains() {
    let mut set = Set(BTreeSet::new());
    set.add(Value::Int(1));
    set.add(Value::Int(1)); // duplicate ignored
    assert!(set.contains(&Value::Int(1)));
    assert!(set.remove(&Value::Int(1)));
    assert!(!set.remove(&Value::Int(1)));
    assert!(!set.contains(&Value::Int(1)));
}
