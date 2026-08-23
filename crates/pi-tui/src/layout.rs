//! Layout constraints — port of `packages/tui/src/layout.ts` (the portion
//! the stack components use): constraint types + a fixed-slice solver.

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LayoutConstraint {
    /// Content-sized (min for text, natural for flex).
    Auto,
    /// Fixed cell count.
    Fixed(u32),
    /// Percentage of the parent (0.0..=1.0).
    Percent(f32),
    /// Grow to fill remaining space.
    Grow,
}

impl LayoutConstraint {
    pub fn fixed(n: u32) -> Self {
        Self::Fixed(n)
    }
    pub fn percent(p: f32) -> Self {
        Self::Percent(p)
    }
}

/// Stack direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackLayout {
    Vertical,
    Horizontal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HStackLayout {
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VStackLayout {
    Top,
    Bottom,
}

/// Solve equal-width flex partitions of `total` for n children with
/// constraints (fixed = their size; percent = fraction; grow = share of
/// remainder). Returns each child's allocated size.
pub fn solve_flex(total: u32, constraints: &[LayoutConstraint]) -> Vec<u32> {
    let mut out = vec![0u32; constraints.len()];
    let mut used = 0u32;
    let mut grows: Vec<usize> = Vec::new();
    for (i, c) in constraints.iter().enumerate() {
        match c {
            LayoutConstraint::Fixed(n) => {
                out[i] = *n;
                used += *n;
            }
            LayoutConstraint::Percent(p) => {
                let n = ((total as f32) * p).floor() as u32;
                out[i] = n;
                used += n;
            }
            LayoutConstraint::Auto | LayoutConstraint::Grow => grows.push(i),
        }
    }
    let remaining = total.saturating_sub(used);
    if !grows.is_empty() {
        let each = remaining / grows.len() as u32;
        let mut extra = remaining % grows.len() as u32;
        for idx in grows {
            out[idx] = each
                + if extra > 0 {
                    extra -= 1;
                    1
                } else {
                    0
                };
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_and_grow_split() {
        let sizes = solve_flex(
            100,
            &[
                LayoutConstraint::Fixed(20),
                LayoutConstraint::Grow,
                LayoutConstraint::Grow,
            ],
        );
        assert_eq!(sizes, vec![20, 40, 40]);
    }

    #[test]
    fn percent_then_grow() {
        let sizes = solve_flex(
            100,
            &[LayoutConstraint::Percent(0.5), LayoutConstraint::Grow],
        );
        assert_eq!(sizes, vec![50, 50]);
    }

    #[test]
    fn all_fixed_no_remainder() {
        let sizes = solve_flex(
            40,
            &[LayoutConstraint::Fixed(10), LayoutConstraint::Fixed(30)],
        );
        assert_eq!(sizes, vec![10, 30]);
        // grow children get zero when nothing remains.
        let sizes = solve_flex(40, &[LayoutConstraint::Fixed(50)]);
        assert_eq!(sizes, vec![50]);
    }
}
