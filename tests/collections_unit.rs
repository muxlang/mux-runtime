//! Unit tests for the collection cores (list, map, set).
#![allow(clippy::mutable_key_type)] // Value keys are logically immutable here.

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
    let s = format!("{list}");
    assert!(s.contains('1') && s.contains('2'));
}

// --- Map ---------------------------------------------------------------------

#[test]
fn map_insert_get_remove_contains() {
    let mut map = Map(mux_runtime::ordered::OrderedMap::new());
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
    let mut set = Set(mux_runtime::ordered::OrderedSet::new());
    set.add(Value::Int(1));
    set.add(Value::Int(1)); // duplicate ignored
    assert!(set.contains(&Value::Int(1)));
    assert!(set.remove(&Value::Int(1)));
    assert!(!set.remove(&Value::Int(1)));
    assert!(!set.contains(&Value::Int(1)));
}

/// Slicing follows the same rules as indexing, which already wraps negatives:
/// half-open, negatives from the end, clamping rather than failing, and empty
/// rather than reversed when the bounds cross.
#[test]
fn list_slice_bounds() {
    use mux_runtime::list::mux_list_slice_value;
    use mux_runtime::refcount::mux_rc_dec;
    use mux_runtime::Value;

    let xs = Value::List(vec![
        Value::Int(1),
        Value::Int(2),
        Value::Int(3),
        Value::Int(4),
        Value::Int(5),
    ]);

    let check = |start: i64, end: i64, want: Vec<i64>| {
        let got = unsafe { mux_list_slice_value(&raw const xs, start, end) };
        let expected = Value::List(want.into_iter().map(Value::Int).collect());
        assert_eq!(
            unsafe { &*got },
            &expected,
            "slice [{start}:{end}] did not match"
        );
        assert!(unsafe { mux_rc_dec(got) });
    };

    check(1, 3, vec![2, 3]);
    check(0, 5, vec![1, 2, 3, 4, 5]);
    check(2, 5, vec![3, 4, 5]);
    check(-2, 5, vec![4, 5]);
    check(0, -1, vec![1, 2, 3, 4]);
    check(2, 99, vec![3, 4, 5]);
    check(-99, 2, vec![1, 2]);
    // Crossed bounds are empty, not reversed.
    check(4, 2, vec![]);
    check(99, 100, vec![]);

    // A non-list is empty rather than a panic - the runtime must not abort a
    // compiled program over a shape it did not expect.
    let not_a_list = Value::Int(7);
    let got = unsafe { mux_list_slice_value(&raw const not_a_list, 0, 1) };
    assert_eq!(unsafe { &*got }, &Value::List(vec![]));
    assert!(unsafe { mux_rc_dec(got) });
}
