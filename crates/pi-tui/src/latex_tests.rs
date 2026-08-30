#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test module
//! Latex renderer tests — ported from `packages/tui/test/latex.test.ts` \
//! (extracted defineCases table).

use crate::latex::render_latex;

fn cases() -> Vec<(&'static str, &'static str)> {
    vec![
            ("\\mathbb{C}^3 \\to \\mathbb{C}^3", "ℂ³ → ℂ³"),
            ("\\{3x+2y,\\; 27x^2-4z-1,\\; x(x-1)(x+1)\\} \\quad\\Rightarrow\\quad x \\in \\{0, \\pm 1\\},", "{3x+2y, 27x²-4z-1, x(x-1)(x+1)} ⇒ x ∈ {0, ± 1},"),
            ("F_1 = -\\frac{1}{4x^2}.", "F₁ = -1/(4x²)."),
            ("-2", "-2"),
            ("(0,0,-1/4)", "(0,0,-1/4)"),
            ("(1,-3/2,13/2)", "(1,-3/2,13/2)"),
            ("(1,1,1)", "(1,1,1)"),
            ("(2,1,0)", "(2,1,0)"),
            ("(-1/4, 0, 0)", "(-1/4, 0, 0)"),
            ("\\{(0,0,-1/4), (1,-3/2,13/2), (-1,3/2,13/2)\\}", "{(0,0,-1/4), (1,-3/2,13/2), (-1,3/2,13/2)}"),
            ("(2,1,1)", "(2,1,1)"),
            ("(7/3,-2/5,11/7)", "(7/3,-2/5,11/7)"),
            ("\\{y - p(x),\\; q(x)\\}", "{y - p(x), q(x)}"),
            ("\\deg q = 3", "deg q = 3"),
            ("[\\mathbb{C}(x,y,z):\\mathbb{C}(F_1,F_2,F_3)] = 3", "[ℂ(x,y,z):ℂ(F₁,F₂,F₃)] = 3"),
            ("u = 1+xy", "u = 1+xy"),
            ("G = u^2 z + y^2(4+3xy)", "G = u² z + y²(4+3xy)"),
            ("F_1 = uG", "F₁ = uG"),
            ("F_2 = y + 3xG", "F₂ = y + 3xG"),
            ("x=0", "x = 0"),
            ("F_2 = F_3 = 0", "F₂ = F₃ = 0"),
            ("xy = -3/2", "xy = -3/2"),
            ("x^2 z = 13/2", "x² z = 13/2"),
            ("\\mathbb{C}^*", "ℂ^*"),
            ("s \\mapsto (s,\\, -\\tfrac{3}{2s},\\, \\tfrac{13}{2s^2})", "s ↦ (s, -3/(2s), 13/(2s²))"),
            ("X", "X"),
            ("p_\\pm", "p_±"),
            ("F(-x,-y,z) = (F_1, -F_2, -F_3)", "F(-x,-y,z) = (F₁, -F₂, -F₃)"),
            ("p_0", "p₀"),
            ("s \\to \\infty", "s → ∞"),
            ("(0,0,0)", "(0,0,0)"),
            ("\\Rightarrow", "⇒"),
            ("\\ge 2", "≥ 2"),
            ("\\ge 3", "≥ 3"),
            ("1", "1"),
            ("\\mathrm{diag}(-1/2,1,1)", "diag(-1/2,1,1)"),
            ("4+3xy", "4+3xy"),
            ("E \\approx \\frac{0.1\\ \\text{lux}}{100\\ \\text{lm/W}} = 0.001\\ \\text{W/m}^2", "E ≈ (0.1 lux)/(100 lm/W) = 0.001 W/m²"),
            ("\\boxed{1\\ \\text{milliwatt per square metre}}", "[1 milliwatt per square metre]"),
            ("5\\ \\text{km}^2 = 5{,}000{,}000\\ \\text{m}^2", "5 km² = 5,000,000 m²"),
            ("P_{\\text{light}} = 0.001 \\times 5{,}000{,}000\n= \\boxed{5{,}000\\ \\text{W}}", "P_light = 0.001 × 5,000,000 = [5,000 W]"),
            ("P_{\\text{electric}} = 5\\ \\text{kW} \\times 0.2\n= \\boxed{1\\ \\text{kW}}", "P_electric = 5 kW × 0.2 = [1 kW]"),
            ("\\pi(2.5\\ \\text{km})^2 = 19.6\\ \\text{km}^2", "π(2.5 km)² = 19.6 km²"),
            ("0.001\\ \\text{W/m}^2 \\times 19.6 \\times 10^6\\ \\text{m}^2\n\\approx \\boxed{20\\ \\text{kW optical}}", "0.001 W/m² × 19.6 × 10⁶ m² ≈ [20 kW optical]"),
            ("1\\ \\text{kW} \\times \\frac{1}{3600}\\ \\text{hour}\n= \\boxed{0.28\\ \\text{Wh}}", "1 kW × 1/3600 hour = [0.28 Wh]"),
            ("\\det\\!\\left(\\frac{\\partial(F_1,F_2,F_3)}{\\partial(x,y,z)}\\right)=-2.", "det((∂(F₁,F₂,F₃))/(∂(x,y,z))) = -2."),
            ("\\begin{aligned}\nF(0,0,-\\tfrac14)&=(-\\tfrac14,0,0),\\\\\nF(1,-\\tfrac32,\\tfrac{13}2)&=(-\\tfrac14,0,0),\\\\\nF(-1,\\tfrac32,\\tfrac{13}2)&=(-\\tfrac14,0,0).\n\\end{aligned}", "F(0,0,-1/4) = (-1/4,0,0),\nF(1,-3/2,13/2) = (-1/4,0,0),\nF(-1,3/2,13/2) = (-1/4,0,0)."),
            ("F=(F_1,F_2,F_3)", "F = (F₁,F₂,F₃)"),
            ("F", "F"),
            ("3", "3"),
            ("J = \\begin{pmatrix}\n\\frac{\\partial f_1}{\\partial x} & \\frac{\\partial f_1}{\\partial y} & \\frac{\\partial f_1}{\\partial z} \\\\\n\\frac{\\partial f_2}{\\partial x} & \\frac{\\partial f_2}{\\partial y} & \\frac{\\partial f_2}{\\partial z} \\\\\n\\frac{\\partial f_3}{\\partial x} & \\frac{\\partial f_3}{\\partial y} & \\frac{\\partial f_3}{\\partial z}\n\\end{pmatrix}", "J = ⎛ (∂ f₁)/(∂ x) │ (∂ f₁)/(∂ y) │ (∂ f₁)/(∂ z) ⎞\n    ⎜ (∂ f₂)/(∂ x) │ (∂ f₂)/(∂ y) │ (∂ f₂)/(∂ z) ⎟\n    ⎝ (∂ f₃)/(∂ x) │ (∂ f₃)/(∂ y) │ (∂ f₃)/(∂ z) ⎠"),
            ("\\begin{aligned}\nf_1 &= (1+xy)^3 z + y^2(1+xy)(4+3xy) \\\\\nf_2 &= y + 3x(1+xy)^2 z + 3xy^2(4+3xy) \\\\\nf_3 &= 2x - 3x^2y - x^3z\n\\end{aligned}", "f₁ = (1+xy)³ z + y²(1+xy)(4+3xy)\nf₂ = y + 3x(1+xy)² z + 3xy²(4+3xy)\nf₃ = 2x - 3x²y - x³z"),
            ("x, y, z", "x, y, z"),
            ("(x, y, z)", "(x, y, z)"),
            ("(0,\\; 0,\\; -\\tfrac14)", "(0, 0, -1/4)"),
            ("(-\\tfrac14,\\; 0,\\; 0)", "(-1/4, 0, 0)"),
            ("(1,\\; -\\tfrac32,\\; \\tfrac{13}{2})", "(1, -3/2, 13/2)"),
            ("(-1,\\; \\tfrac32,\\; \\tfrac{13}{2})", "(-1, 3/2, 13/2)"),
            ("(-\\frac14, 0, 0)", "(-1/4, 0, 0)"),
            ("F: \\mathbb{C}^3 \\to \\mathbb{C}^3", "F: ℂ³ → ℂ³"),
            ("F(0,0,-\\tfrac14) = F(1,-\\tfrac32,\\tfrac{13}{2}) = F(-1,\\tfrac32,\\tfrac{13}{2}) = (-\\tfrac14, 0, 0)", "F(0,0,-1/4) = F(1,-3/2,13/2) = F(-1,3/2,13/2) = (-1/4, 0, 0)"),
            ("\\mathbb{C}^3", "ℂ³"),
            ("\\begin{aligned}\nf_1 &= \\frac{f_1^{\\text{ut}}(u,t)}{x^2}, \\quad\nf_2 = \\frac{f_2^{\\text{ut}}(u,t)}{x}, \\quad\nf_3 = x\\,(2 - 3u - t)\n\\end{aligned}", "f₁ = (f₁ᵘᵗ(u,t))/(x²), f₂ = (f₂ᵘᵗ(u,t))/x, f₃ = x (2 - 3u - t)"),
            ("\\det J_F", "det J_F"),
            ("(-\\tfrac14, 0, 0)", "(-1/4, 0, 0)"),
            ("u = xy", "u = xy"),
            ("t = x^2z", "t = x²z"),
            ("x \\neq 0", "x ≠ 0"),
            ("f_1^{\\text{ut}}, f_2^{\\text{ut}}", "f₁ᵘᵗ, f₂ᵘᵗ"),
            ("u,t", "u,t"),
            ("x", "x"),
            ("x, x^2", "x, x²"),
            ("\\mathbb{C}^n \\to \\mathbb{C}^n", "ℂⁿ → ℂⁿ"),
            ("n \\geq 2", "n ≥ 2"),
            ("\\mathbb{P}^3", "ℙ³"),
            ("e^{i\\pi}+1=0", "e^(iπ)+1 = 0"),
            ("\\boxed{\n\\mathcal{Z}(\\beta)\n=\n\\int_{\\mathcal M}\n\\exp\\!\\left(\n-\\beta\\left[\n\\frac12 g^{ij}(x)\\,\\partial_i\\phi\\,\\partial_j\\phi\n+V(\\phi)\n\\right]\\right)\n\\mathcal D\\phi\n}", "[Z(β) = ∫_M exp( -β[ 1/2 gⁱʲ(x) ∂ᵢϕ ∂ⱼϕ +V(ϕ) ]) Dϕ]"),
            ("\\begin{aligned}\n\\nabla_\\mu T^{\\mu\\nu}\n&=\n\\frac{1}{\\sqrt{-g}}\n\\partial_\\mu\\!\\left(\\sqrt{-g}\\,T^{\\mu\\nu}\\right)\n+\\Gamma^\\nu_{\\mu\\lambda}T^{\\mu\\lambda}\n=0, \\\\[4pt]\nR_{\\mu\\nu}-\\frac12 Rg_{\\mu\\nu}+\\Lambda g_{\\mu\\nu}\n&=\n\\frac{8\\pi G}{c^4}T_{\\mu\\nu}.\n\\end{aligned}", "∇_μ T^(μν) = 1/(√(-g)) ∂_μ(√(-g) T^(μν)) +Γ^ν_(μλ)T^(μλ) = 0,\nR_(μν)-1/2 Rg_(μν)+Λ g_(μν) = (8π G)/(c⁴)T_(μν)."),
            ("\\Psi(x,t)=\n\\sum_{n=1}^{\\infty}\n\\underbrace{\nc_n\n\\sqrt{\\frac{2}{L}}\n\\sin\\!\\left(\\frac{n\\pi x}{L}\\right)\n}_{\\text{spatial eigenmode}}\n\\exp\\!\\left(-\\frac{i\\hbar n^2\\pi^2}{2mL^2}t\\right),\n\\qquad\n|\\Psi(x,t)|^2\n=\n\\begin{cases}\n\\Psi^\\ast\\Psi, & 0<x<L,\\\\\n0, & \\text{otherwise}.\n\\end{cases}", "Ψ(x,t) = ∑ₙ₌₁^∞ cₙ √(2/L) sin((nπ x)/L)_(spatial eigenmode) exp(-(iℏ n²π²)/(2mL²)t), |Ψ(x,t)|² = ⎧ Ψ^∗Ψ if 0 < x < L,\n⎩ 0 otherwise."),
            ("x=\\frac{-b\\pm\\sqrt{b^2-4ac}}{2a}", "x = (-b±√(b²-4ac))/(2a)"),
            ("\\int_0^\\infty e^{-x^2}\\,dx=\\frac{\\sqrt{\\pi}}{2}", "∫₀^∞ e^(-x²) dx = (√π)/2"),
            ("e^{i\\theta}=\\cos\\theta+i\\sin\\theta", "e^(iθ) = cos θ+i sin θ"),
            ("\\sum_{n=1}^{\\infty}\\frac{1}{n^2}=\\frac{\\pi^2}{6}", "∑ₙ₌₁^∞1/(n²) = π²/6"),
            ("\\lim_{x\\to 0}\\frac{\\sin x}{x}=1", "lim[x→0] (sin x)/x = 1"),
            ("\\lim_{n\\to\\infty}\n\\left(1+\\frac{1}{n}\\right)^n=e", "lim[n→∞] (1+1/n)ⁿ = e"),
            ("\\int_0^1 \\frac{x^2}{1+x^3}\\,dx\n=\\frac{1}{3}\\ln 2", "∫₀¹ x²/(1+x³) dx = 1/3 ln 2"),
            ("\\sum_{k=1}^{n}\\frac{k}{k+1}\n=n+1-H_{n+1}", "∑ₖ₌₁ⁿk/(k+1) = n+1-Hₙ₊₁"),
            ("\\frac{\n  \\displaystyle \\frac{x^2+1}{x-1}\n  -\n  \\displaystyle \\frac{2x}{x+1}\n}{\n  \\displaystyle \\frac{x}{x^2-1}\n}", "((x²+1)/(x-1) - 2x/(x+1))/(x/(x²-1))"),
            ("\\lim_{x\\to 0}\n\\frac{\n  \\displaystyle \\frac{\\sin x}{x}-1\n}{\n  \\displaystyle \\frac{e^x-1}{x}-1\n}\n=0", "lim[x→0] ((sin x)/x-1)/((eˣ-1)/x-1) = 0"),
            ("\\frac{\n  1+\\displaystyle\\frac{1}{1+\\frac{1}{x}}\n}{\n  1-\\displaystyle\\frac{1}{1-\\frac{1}{x}}\n}", "(1+1/(1+1/x))/(1-1/(1-1/x))"),
            ("\\sum_{n=1}^{\\infty}\n\\frac{\n  \\displaystyle \\frac{1}{n}-\\frac{1}{n+1}\n}{\n  \\displaystyle 1+\\frac{1}{n^2}\n}", "∑ₙ₌₁^∞ (1/n-1/(n+1))/(1+1/(n²))"),
        ]
}

#[test]
fn renders_latex_cases() {
    let cases = cases();
    let mut failed = 0;
    for (i, (source, expected)) in cases.iter().enumerate() {
        let actual = render_latex(source, false);
        if actual.as_deref() != Some(*expected) {
            failed += 1;
            eprintln!(
                "[{}] source: {:?}\n  expected: {:?}\n  actual:   {:?}",
                i, source, expected, actual
            );
        }
    }
    assert_eq!(failed, 0, "{failed} latex cases mismatched");
}

#[test]
fn display_mode_stacks_fractions_and_limits() {
    // Stacked fraction in display mode.
    let stacked = render_latex("\\frac{1}{2}", true);
    assert!(stacked.as_deref().unwrap_or("").contains("─"));
    // Inline frac is not stacked.
    let inline = render_latex("\\frac{1}{2}", false).unwrap();
    assert_eq!(inline, "1/2");
    // Operator limits in display mode produce a layout block.
    let limit = render_latex("\\sum_{n=1}^{\\infty}", true).unwrap();
    assert!(limit.contains("∑"));
}

#[test]
fn unsupported_syntax_returns_none() {
    assert!(render_latex("\\unknowncmd", false).is_none());
    assert!(render_latex("\\frac{1", false).is_none());
}

#[test]
fn keeps_unicode_offsets_byte_safe_in_text_and_groups() {
    assert_eq!(render_latex("α + β", false).as_deref(), Some("α + β"));
    assert_eq!(render_latex(r"\text{π}", false).as_deref(), Some("π"));
    assert_eq!(render_latex(r"\frac{π}{2}", false).as_deref(), Some("π/2"));
    assert_eq!(render_latex("α\u{2003}β", false).as_deref(), Some("α β"));
}

#[test]
fn malformed_unicode_groups_fail_without_boundary_panics() {
    assert!(render_latex(r"\text{π", false).is_none());
    assert!(render_latex(r"\frac{π", false).is_none());
}

#[test]
fn cleans_unicode_matrix_layout_markers_after_rendering() {
    assert_eq!(
        render_latex(r"\begin{pmatrix}α&β\\γ&δ\end{pmatrix}", false).as_deref(),
        Some("⎛ α │ β ⎞\n⎝ γ │ δ ⎠")
    );
}

#[test]
fn matches_upstream_case_condition_casing_and_word_boundaries() {
    assert_eq!(
        render_latex(
            r"\begin{cases}a & IF x=0 \\ b & otherwise.\end{cases}",
            false,
        )
        .as_deref(),
        Some("⎧ a IF x = 0\n⎩ b otherwise.")
    );
}

#[test]
fn only_applies_limits_when_adjacent_to_an_operator() {
    assert_eq!(
        render_latex(r"\sum x\limits_{n}", true).as_deref(),
        Some("∑ xₙ")
    );
}

#[test]
fn normalizes_whitespace_around_script_operators() {
    assert_eq!(render_latex(r"x_{i = 0}", false).as_deref(), Some("xᵢ₌₀"));
}

#[test]
fn attaches_periods_only_to_terminal_matrix_layout_markers() {
    let mut nodes = Vec::new();
    {
        let mut parser =
            super::LatexParser::new(r"\begin{pmatrix}a\\b\end{pmatrix}.", &mut nodes, false);
        parser.render().expect("matrix should render");
    }
    assert!(matches!(
        nodes.first(),
        Some(super::LayoutNode::Matrix { lines, .. })
            if lines.last().is_some_and(|line| line.ends_with('.'))
    ));

    let mut nodes = Vec::new();
    {
        let mut parser =
            super::LatexParser::new(r"\begin{pmatrix}a\\b\end{pmatrix} x.", &mut nodes, false);
        parser.render().expect("matrix should render");
    }
    assert!(matches!(
        nodes.first(),
        Some(super::LayoutNode::Matrix { lines, .. })
            if lines.last().is_some_and(|line| !line.ends_with('.'))
    ));
}

#[test]
fn matches_upstream_extended_latex_oracles() {
    let cases = [
        (
            r"\sum_{i=0}^n \alpha_i + \int_0^\infty e^{-x^2}\,dx = \sqrt{\pi}",
            "∑ᵢ₌₀ⁿ αᵢ + ∫₀^∞ e^(-x²) dx = √π",
        ),
        (
            r"\binom{n}{k}+\vec{x}+\hat{y}+\overline{AB}",
            "(n choose k)+x⃗+ŷ+overline(AB)",
        ),
        (r"A\not\subseteq B,\quad x\not\in X", "A ⊈ B, x ∉ X"),
        (
            r"\sqrt[2]{x}+\sqrt[3]{x}+\sqrt[4]{x}+\sqrt[n]{x}+\sqrt[k]{x+1}",
            "√x+∛x+∜x+ⁿ√x+ᵏ√(x+1)",
        ),
        (
            r"\begin{equation}\begin{split}a&=b\\&=c\end{split}\end{equation}",
            "a = b\n= c",
        ),
        (
            r"\begin{cases}a & x<0 \\ b & \text{if }x=0 \\ c & \text{otherwise}\end{cases}",
            "⎧ a if x < 0\n⎨ b if x = 0\n⎩ c otherwise",
        ),
        (
            r"\begin{pmatrix}1&200\\3000&4\end{pmatrix}",
            "⎛ 1    │ 200 ⎞\n⎝ 3000 │ 4   ⎠",
        ),
    ];
    for (source, expected) in cases {
        assert_eq!(
            render_latex(source, false).as_deref(),
            Some(expected),
            "{source}"
        );
    }
}

#[test]
fn malformed_latex_inputs_remain_total_and_rejected() {
    for source in [
        r"\frac{1}{x",
        r"x}",
        r"\begin{matrix}1 & 2",
        "x\\",
        "\\text{π",
        "\\sqrt[α",
    ] {
        assert!(render_latex(source, false).is_none(), "{source:?}");
    }
}
