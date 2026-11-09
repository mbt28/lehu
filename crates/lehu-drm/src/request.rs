// SPDX-License-Identifier: GPL-3.0-or-later
//! One atomic request, reused: fixed arrays instead of a growing property
//! list, so a commit allocates nothing. The properties of one object stay
//! together, as the ioctl wants them.

/// Connector, CRTC and two planes.
pub const MAX_OBJECTS: usize = 4;
/// A modeset (3) and two planes with their geometry and zpos (11 each) is
/// 25; the rest is slack.
pub const MAX_PROPS: usize = 32;

#[derive(Debug)]
pub struct Request {
    objects: [u32; MAX_OBJECTS],
    counts: [u32; MAX_OBJECTS],
    props: [u32; MAX_PROPS],
    values: [u64; MAX_PROPS],
    object_count: usize,
    prop_count: usize,
    /// Something did not fit: the request is not to be committed.
    overflow: bool,
}

impl Default for Request {
    fn default() -> Self {
        Self::new()
    }
}

impl Request {
    pub const fn new() -> Self {
        Self {
            objects: [0; MAX_OBJECTS],
            counts: [0; MAX_OBJECTS],
            props: [0; MAX_PROPS],
            values: [0; MAX_PROPS],
            object_count: 0,
            prop_count: 0,
            overflow: false,
        }
    }

    pub fn clear(&mut self) {
        self.object_count = 0;
        self.prop_count = 0;
        self.overflow = false;
    }

    pub fn overflowed(&self) -> bool {
        self.overflow
    }

    /// Set `prop` of `object` to `value`. A second value for the same pair
    /// replaces the first.
    pub fn add(&mut self, object: u32, prop: u32, value: u64) {
        let (index, start) = match self.objects[..self.object_count]
            .iter()
            .position(|&o| o == object)
        {
            Some(i) => {
                let start: u32 = self.counts[..i].iter().sum();
                (i, start as usize)
            }
            None => {
                if self.object_count == MAX_OBJECTS {
                    self.overflow = true;
                    return;
                }
                self.objects[self.object_count] = object;
                self.counts[self.object_count] = 0;
                self.object_count += 1;
                // A new object's run begins where the last one ends.
                (self.object_count - 1, self.prop_count)
            }
        };
        let end = start + self.counts[index] as usize;
        if let Some(i) = self.props[start..end].iter().position(|&p| p == prop) {
            self.values[start + i] = value;
            return;
        }
        if self.prop_count == MAX_PROPS {
            self.overflow = true;
            return;
        }
        // Make room at the end of this object's run.
        self.props.copy_within(end..self.prop_count, end + 1);
        self.values.copy_within(end..self.prop_count, end + 1);
        self.props[end] = prop;
        self.values[end] = value;
        self.counts[index] += 1;
        self.prop_count += 1;
    }

    /// The arrays as the ioctl takes them: objects, properties per object,
    /// properties, values.
    pub fn parts(&mut self) -> (&mut [u32], &mut [u32], &mut [u32], &mut [u64]) {
        let Self {
            objects,
            counts,
            props,
            values,
            object_count,
            prop_count,
            ..
        } = self;
        (
            &mut objects[..*object_count],
            &mut counts[..*object_count],
            &mut props[..*prop_count],
            &mut values[..*prop_count],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(r: &mut Request) -> (Vec<u32>, Vec<u32>, Vec<u32>, Vec<u64>) {
        let (o, c, p, v) = r.parts();
        (o.to_vec(), c.to_vec(), p.to_vec(), v.to_vec())
    }

    #[test]
    fn properties_stay_with_their_object() {
        let mut r = Request::new();
        r.add(10, 1, 100);
        r.add(20, 1, 200);
        r.add(10, 2, 101);
        r.add(20, 2, 201);
        r.add(30, 1, 300);
        let (objects, counts, props, values) = snapshot(&mut r);
        assert_eq!(objects, vec![10, 20, 30]);
        assert_eq!(counts, vec![2, 2, 1]);
        assert_eq!(props, vec![1, 2, 1, 2, 1]);
        assert_eq!(values, vec![100, 101, 200, 201, 300]);
        assert!(!r.overflowed());
    }

    #[test]
    fn a_second_value_replaces_the_first() {
        let mut r = Request::new();
        r.add(10, 1, 100);
        r.add(20, 1, 200);
        r.add(10, 1, 111);
        let (objects, counts, props, values) = snapshot(&mut r);
        assert_eq!(objects, vec![10, 20]);
        assert_eq!(counts, vec![1, 1]);
        assert_eq!(props, vec![1, 1]);
        assert_eq!(values, vec![111, 200]);
    }

    #[test]
    fn clear_starts_over() {
        let mut r = Request::new();
        r.add(10, 1, 100);
        r.clear();
        let (objects, counts, props, values) = snapshot(&mut r);
        assert!(objects.is_empty() && counts.is_empty());
        assert!(props.is_empty() && values.is_empty());
        r.add(20, 5, 500);
        let (objects, _, props, _) = snapshot(&mut r);
        assert_eq!(objects, vec![20]);
        assert_eq!(props, vec![5]);
    }

    #[test]
    fn too_much_is_flagged_not_written() {
        let mut r = Request::new();
        for o in 0..MAX_OBJECTS as u32 {
            r.add(o + 1, 1, 0);
        }
        assert!(!r.overflowed());
        r.add(99, 1, 0);
        assert!(r.overflowed());
        assert_eq!(snapshot(&mut r).0.len(), MAX_OBJECTS);

        let mut r = Request::new();
        for p in 0..MAX_PROPS as u32 {
            r.add(1, p + 1, u64::from(p));
        }
        assert!(!r.overflowed());
        r.add(1, 1000, 0);
        assert!(r.overflowed());
        let (_, counts, props, _) = snapshot(&mut r);
        assert_eq!(counts, vec![MAX_PROPS as u32]);
        assert_eq!(props.len(), MAX_PROPS);
        // Replacing a value still fits.
        r.clear();
        for p in 0..MAX_PROPS as u32 {
            r.add(1, p + 1, 0);
        }
        r.add(1, 1, 7);
        assert!(!r.overflowed());
        assert_eq!(snapshot(&mut r).3[0], 7);
    }
}
