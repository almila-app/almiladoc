use crate::shared::text::trim_tex;

#[derive(Default)]
pub(super) struct Tex {
    out: String,
    unresolved: bool,
    shape: Shape,
}

/// What the guards need to know about the output, kept as it is written so none reads it back.
#[derive(Default)]
struct Shape {
    /// The levels around the innermost open group, each with the mark that group is the
    /// argument of.
    outer: Vec<(Level, Option<char>)>,
    level: Level,
    escaped: bool,
    /// The output ends in a letter-named control word, which a letter written next would extend.
    in_word: bool,
    /// The first character other than a plain space, and where it is.
    lead: Option<(char, usize)>,
    chars: u8,
    word: Word,
    close_bracket: bool,
    ampersand: bool,
}

/// The last atom written at one nesting level.
#[derive(Default, Clone, Copy)]
struct Level {
    /// A `^` or `_` waiting for its argument.
    pending: Option<char>,
    sub: bool,
    sup: bool,
    /// The superscript is primes, which merge with a prime or `^` written next.
    primed: bool,
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum Word {
    #[default]
    Empty,
    Backslash,
    Letters,
    Other,
}

impl Level {
    fn atom(&mut self) {
        *self = Self::default();
    }

    fn token(&mut self) {
        match self.pending.take() {
            Some(mark) => self.script(mark),
            None => self.atom(),
        }
    }

    fn script(&mut self, mark: char) {
        if mark == '^' {
            self.sup = true;
        } else {
            self.sub = true;
        }
        self.primed = false;
    }
}

impl Shape {
    fn step(&mut self, c: char, at: usize) {
        self.chars = self.chars.saturating_add(1).min(2);
        self.word = match (self.word, c) {
            (Word::Empty, '\\') => Word::Backslash,
            (Word::Backslash | Word::Letters, c) if c.is_ascii_alphabetic() => Word::Letters,
            _ => Word::Other,
        };
        if self.lead.is_none() && c != ' ' {
            self.lead = Some((c, at));
        }
        self.close_bracket |= c == ']';
        self.ampersand |= c == '&';
        if self.escaped {
            self.escaped = false;
            self.in_word = c.is_ascii_alphabetic();
            return;
        }
        if self.in_word && c.is_ascii_alphabetic() {
            return;
        }
        self.in_word = false;
        match c {
            '\\' => {
                self.escaped = true;
                self.level.token();
            }
            '{' => {
                let mark = self.level.pending.take();
                self.outer.push((std::mem::take(&mut self.level), mark));
            }
            '}' => {
                if let Some((level, mark)) = self.outer.pop() {
                    self.level = level;
                    match mark {
                        Some(mark) => self.level.script(mark),
                        None => self.level.atom(),
                    }
                }
            }
            ' ' => {}
            '^' | '_' => self.level.pending = Some(c),
            '\'' => {
                self.level.sup = true;
                self.level.primed = true;
            }
            _ => self.level.token(),
        }
    }

    fn grouped(&mut self, start: usize) {
        self.level.atom();
        (self.escaped, self.in_word, self.chars, self.word) = (false, false, 2, Word::Other);
        if self.lead.is_none_or(|(_, at)| start <= at) {
            self.lead = Some(('{', start));
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum Fraction {
    Bar,
    Linear,
    Skewed,
    NoBar,
}

impl Tex {
    pub(super) fn without_unresolved(mut self) -> Self {
        self.unresolved = false;
        self
    }

    pub(super) fn is_empty(&self) -> bool {
        self.out.is_empty()
    }

    pub(super) fn len(&self) -> usize {
        self.out.len()
    }

    pub(super) fn holds_ampersand(&self) -> bool {
        self.shape.ampersand
    }

    fn write(&mut self, s: &str) {
        let start = self.out.len();
        for (offset, c) in s.char_indices() {
            self.shape.step(c, start + offset);
        }
        self.out.push_str(s);
    }

    pub(super) fn push_str(&mut self, s: &str) {
        let Some(first) = s.chars().next() else { return };
        if self.shape.in_word && first.is_ascii_alphabetic() {
            self.write(" ");
        }
        self.write(s);
    }

    pub(super) fn push_char(&mut self, c: char) {
        self.push_str(c.encode_utf8(&mut [0; 4]));
    }

    /// The backslash ends any control word before it, so no separator is needed.
    pub(super) fn push_macro(&mut self, name: &str) {
        self.write(name);
    }

    pub(super) fn push_tex(&mut self, other: &Self) {
        if other.out.is_empty() {
            return;
        }
        if other.raises() && self.carries_superscript() {
            self.push_str("{}");
        }
        self.unresolved |= other.unresolved;
        self.push_str(&other.out);
    }

    pub(super) fn push_group(&mut self, inner: &Self) {
        self.push_char('{');
        self.push_tex(inner);
        self.push_char('}');
    }

    pub(super) fn push_script(&mut self, mark: char, inner: &Self) {
        self.push_char(mark);
        self.push_group(inner);
    }

    /// Only plain spaces, which math mode ignores, were written.
    pub(super) fn is_blank(&self) -> bool {
        self.shape.lead.is_none()
    }

    /// It opens, after plain spaces, with a prime or a superscript, which attach to the atom
    /// written before it; a control space `\ ` opens with its backslash and is an atom.
    pub(super) fn raises(&self) -> bool {
        self.shape.lead.is_some_and(|(c, _)| c == '\'' || c == '^')
    }

    /// It opens, after plain spaces, with a `[` or `*` that a `\\` or `\begin{..}` before it
    /// would read as its own optional argument.
    fn opens_option(&self) -> bool {
        self.shape.lead.is_some_and(|(c, _)| c == '[' || c == '*')
    }

    /// Its last atom ends in a script or a prime, so a script attached directly would double one.
    pub(super) fn ends_with_script(&self) -> bool {
        self.shape.level.sub || self.shape.level.sup
    }

    /// A prime or superscript written next would give its last atom a second superscript: it
    /// has one already, unless that ends in primes, which merge with what follows.
    pub(super) fn carries_superscript(&self) -> bool {
        self.shape.level.sup && !self.shape.level.primed
    }

    /// Groups what was written from `start`, shifting it by the brace opened before it.
    pub(super) fn group_from(&mut self, start: usize) {
        self.out.insert(start, '{');
        self.out.push('}');
        self.shape.grouped(start);
    }

    /// `\sqrt`, then `[index]` braced when it holds a `]`, which would end the argument early.
    pub(super) fn push_sqrt(&mut self, index: Option<&Self>) {
        self.push_macro("\\sqrt");
        if let Some(index) = index {
            self.push_char('[');
            if index.shape.close_bracket {
                self.push_group(index);
            } else {
                self.push_tex(index);
            }
            self.push_char(']');
        }
    }

    pub(super) fn push_fraction(&mut self, kind: Fraction, num: &Self, den: &Self) {
        match kind {
            Fraction::Linear => {
                self.push_group(num);
                self.push_char('/');
                self.push_group(den);
            }
            Fraction::Skewed => {
                self.push_str("{}^");
                self.push_group(num);
                self.push_str("/_");
                self.push_group(den);
            }
            Fraction::NoBar => {
                self.push_char('{');
                self.push_tex(num);
                self.push_macro("\\atop");
                self.push_tex(den);
                self.push_char('}');
            }
            Fraction::Bar => {
                self.push_macro("\\frac");
                self.push_group(num);
                self.push_group(den);
            }
        }
    }

    pub(super) fn push_environment(&mut self, env: &str, rows: &[Vec<Self>]) {
        self.push_str(&format!("\\begin{{{env}}}"));
        for (i, row) in rows.iter().enumerate() {
            if i > 0 {
                self.push_str(" \\\\");
            }
            for (j, cell) in row.iter().enumerate() {
                self.push_str(if j > 0 { " & " } else { " " });
                if j == 0 {
                    self.push_after_option(cell);
                } else {
                    self.push_tex(cell);
                }
            }
        }
        self.push_str(&format!(" \\end{{{env}}}"));
    }

    /// The first piece after `\\` or `\begin{..}`, which read a `[` or `*` that follows as
    /// their own.
    fn push_after_option(&mut self, piece: &Self) {
        if piece.opens_option() {
            self.push_str("{}");
        }
        self.push_tex(piece);
    }

    /// A script base: `inner` as it stands when it is one atom (a single
    /// character or control word), `{inner}` otherwise.
    pub(super) fn push_base(&mut self, inner: &Self) {
        let atom = (inner.shape.chars == 1 || inner.shape.word == Word::Letters)
            && !inner.ends_with_script();
        if atom { self.push_tex(inner) } else { self.push_group(inner) }
    }

    pub(super) fn push_command(&mut self, name: &str, inner: &Self) {
        self.push_macro(name);
        self.push_group(inner);
    }

    pub(super) fn push_math_text(&mut self, text: &str) {
        let mut pending: Option<(Variant, String)> = None;
        for c in text.chars() {
            let (base, variant) = fold_alnum(c).unwrap_or((c, Variant::Plain));
            if variant == Variant::Plain {
                self.flush_variant(pending.take());
                self.push_math_char(base);
            } else {
                match &mut pending {
                    Some((v, run)) if *v == variant => run.push(base),
                    _ => {
                        self.flush_variant(pending.take());
                        pending = Some((variant, base.to_string()));
                    }
                }
            }
        }
        self.flush_variant(pending.take());
    }

    fn flush_variant(&mut self, pending: Option<(Variant, String)>) {
        let Some((variant, run)) = pending else { return };
        let mut inner = Self::default();
        inner.push_math_text(&run);
        self.push_command(variant.command(), &inner);
    }

    pub(super) fn push_math_char(&mut self, c: char) {
        if raises(c) && self.carries_superscript() {
            self.push_str("{}");
        }
        if let Some(mapped) = symbol(c) {
            if mapped.starts_with('\\') {
                self.push_macro(mapped);
            } else {
                self.push_str(mapped);
            }
            return;
        }
        match c {
            '{' | '}' | '$' | '%' | '&' | '#' | '_' => {
                self.push_char('\\');
                self.push_char(c);
            }
            '\\' => self.push_macro("\\backslash"),
            '^' => {
                self.push_str("\\char`^");
                self.shape.level.atom();
            }
            '~' => self.push_macro("\\sim"),
            '\u{a0}' => self.push_str("\\ "),
            c if c.is_whitespace() => self.push_char(' '),
            c if c.is_control() => {}
            c => self.push_char(c),
        }
    }

    pub(super) fn push_text_mode(&mut self, text: &str) {
        let mut inner = Self::default();
        for c in text.chars() {
            match c {
                '{' | '}' | '$' | '%' | '&' | '#' | '_' => {
                    inner.push_char('\\');
                    inner.push_char(c);
                }
                '\\' => inner.push_str("\\textbackslash{}"),
                '^' => inner.push_str("\\textasciicircum{}"),
                '~' => inner.push_str("\\textasciitilde{}"),
                '\u{a0}' => inner.push_char('~'),
                c if c.is_whitespace() => inner.push_char(' '),
                c if c.is_control() => {}
                c => inner.push_char(c),
            }
        }
        self.push_command("\\text", &inner);
    }

    pub(super) fn mark_unresolved(&mut self, unresolved: bool) {
        self.unresolved |= unresolved;
    }

    pub(super) fn finish_with_damage(self) -> (String, bool) {
        let damaged = self.unresolved;
        (self.finish(), damaged)
    }

    pub(super) fn finish(self) -> String {
        let mut out = String::with_capacity(self.out.len());
        let mut prev_space = true;
        for c in trim_tex(&self.out).chars() {
            let space = c == ' ';
            if !(space && prev_space) {
                out.push(c);
            }
            prev_space = space;
        }
        out
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Variant {
    Plain,
    Bold,
    BoldItalic,
    Script,
    Fraktur,
    DoubleStruck,
    SansSerif,
    Monospace,
}

impl Variant {
    pub(super) fn command(self) -> &'static str {
        match self {
            Self::Plain => "\\mathit",
            Self::Bold => "\\mathbf",
            Self::BoldItalic => "\\boldsymbol",
            Self::Script => "\\mathcal",
            Self::Fraktur => "\\mathfrak",
            Self::DoubleStruck => "\\mathbb",
            Self::SansSerif => "\\mathsf",
            Self::Monospace => "\\mathtt",
        }
    }

    pub(super) fn from_mathvariant(value: &str) -> Option<Self> {
        Some(match value {
            "bold" => Self::Bold,
            "bold-italic" => Self::BoldItalic,
            "script" | "bold-script" => Self::Script,
            "fraktur" | "bold-fraktur" => Self::Fraktur,
            "double-struck" => Self::DoubleStruck,
            "sans-serif" | "bold-sans-serif" | "sans-serif-italic" | "sans-serif-bold-italic" => {
                Self::SansSerif
            }
            "monospace" => Self::Monospace,
            _ => return None,
        })
    }
}

const GREEK_UPPER: &str = "ΑΒΓΔΕΖΗΘΙΚΛΜΝΞΟΠΡϴΣΤΥΦΧΨΩ";
const GREEK_LOWER: &str = "αβγδεζηθικλμνξοπρςστυφχψω";
const GREEK_EXTRA: &str = "∂ϵϑϰϕϱϖ";

/// Italic is the default math style, so it folds to `Plain`.
fn fold_alnum(c: char) -> Option<(char, Variant)> {
    let cp = u32::from(c);
    fold_letterlike(c)
        .or_else(|| fold_latin(cp))
        .or_else(|| fold_greek(cp))
        .or_else(|| fold_digit(cp))
}

fn fold_letterlike(c: char) -> Option<(char, Variant)> {
    use Variant::{DoubleStruck, Fraktur, Plain, Script};
    Some(match c {
        'ℎ' => ('h', Plain),
        'ℬ' => ('B', Script),
        'ℰ' => ('E', Script),
        'ℱ' => ('F', Script),
        'ℋ' => ('H', Script),
        'ℐ' => ('I', Script),
        'ℒ' => ('L', Script),
        'ℳ' => ('M', Script),
        'ℛ' => ('R', Script),
        'ℯ' => ('e', Script),
        'ℊ' => ('g', Script),
        'ℴ' => ('o', Script),
        'ℭ' => ('C', Fraktur),
        'ℌ' => ('H', Fraktur),
        'ℑ' => ('I', Fraktur),
        'ℜ' => ('R', Fraktur),
        'ℨ' => ('Z', Fraktur),
        'ℂ' => ('C', DoubleStruck),
        'ℍ' => ('H', DoubleStruck),
        'ℕ' => ('N', DoubleStruck),
        'ℙ' => ('P', DoubleStruck),
        'ℚ' => ('Q', DoubleStruck),
        'ℝ' => ('R', DoubleStruck),
        'ℤ' => ('Z', DoubleStruck),
        _ => return None,
    })
}

fn fold_latin(cp: u32) -> Option<(char, Variant)> {
    use Variant::{Bold, BoldItalic, DoubleStruck, Fraktur, Monospace, Plain, SansSerif, Script};
    const STYLES: [Variant; 13] = [
        Bold,
        Plain,
        BoldItalic,
        Script,
        Script,
        Fraktur,
        DoubleStruck,
        Fraktur,
        SansSerif,
        SansSerif,
        SansSerif,
        SansSerif,
        Monospace,
    ];
    let offset = cp.checked_sub(0x1D400).filter(|&o| o < 13 * 52)?;
    let (style, i) = ((offset / 52) as usize, offset % 52);
    let base =
        if i < 26 { b'A' + u8::try_from(i).ok()? } else { b'a' + u8::try_from(i - 26).ok()? };
    Some((base as char, STYLES[style]))
}

fn fold_greek(cp: u32) -> Option<(char, Variant)> {
    use Variant::{Bold, BoldItalic, Plain, SansSerif};
    const STYLES: [Variant; 5] = [Bold, Plain, BoldItalic, SansSerif, SansSerif];
    let greek = cp.checked_sub(0x1D6A8).filter(|&g| g < 5 * 58)?;
    let (style, i) = ((greek / 58) as usize, (greek % 58) as usize);
    let base = match i {
        0..=24 => GREEK_UPPER.chars().nth(i)?,
        25 => '∇',
        26..=50 => GREEK_LOWER.chars().nth(i - 26)?,
        _ => GREEK_EXTRA.chars().nth(i - 51)?,
    };
    Some((base, STYLES[style]))
}

fn fold_digit(cp: u32) -> Option<(char, Variant)> {
    use Variant::{Bold, DoubleStruck, Monospace, SansSerif};
    const STYLES: [Variant; 5] = [Bold, DoubleStruck, SansSerif, SansSerif, Monospace];
    let digit = cp.checked_sub(0x1D7CE).filter(|&d| d < 5 * 10)?;
    Some(((b'0' + (digit % 10) as u8) as char, STYLES[(digit / 10) as usize]))
}

fn raises(c: char) -> bool {
    c == '\'' || symbol(c).is_some_and(|mapped| mapped.starts_with(['\'', '^']))
}

#[rustfmt::skip]
pub(super) fn big_operator(c: char) -> Option<&'static str> {
    Some(match c {
        '∑' => "\\sum", '∏' => "\\prod", '∐' => "\\coprod", '∫' => "\\int",
        '∬' => "\\iint", '∭' => "\\iiint", '⨌' => "\\iiiint", '∮' => "\\oint",
        '∯' => "\\oiint", '∰' => "\\oiiint", '⋃' => "\\bigcup", '⋂' => "\\bigcap",
        '⋁' => "\\bigvee", '⋀' => "\\bigwedge", '⨁' => "\\bigoplus", '⨂' => "\\bigotimes",
        '⨀' => "\\bigodot", '⨄' => "\\biguplus", '⨆' => "\\bigsqcup",
        _ => return None,
    })
}

#[rustfmt::skip]
pub(super) fn symbol(c: char) -> Option<&'static str> {
    big_operator(c).or_else(|| Some(match c {
        'α' => "\\alpha", 'β' => "\\beta", 'γ' => "\\gamma", 'δ' => "\\delta",
        'ε' => "\\varepsilon", 'ϵ' => "\\epsilon", 'ζ' => "\\zeta", 'η' => "\\eta",
        'θ' => "\\theta", 'ϑ' => "\\vartheta", 'ι' => "\\iota", 'κ' => "\\kappa",
        'ϰ' => "\\varkappa", 'λ' => "\\lambda", 'μ' => "\\mu", 'ν' => "\\nu",
        'ξ' => "\\xi", 'ο' => "o", 'π' => "\\pi", 'ϖ' => "\\varpi",
        'ρ' => "\\rho", 'ϱ' => "\\varrho", 'σ' => "\\sigma", 'ς' => "\\varsigma",
        'τ' => "\\tau", 'υ' => "\\upsilon", 'φ' => "\\varphi", 'ϕ' => "\\phi",
        'χ' => "\\chi", 'ψ' => "\\psi", 'ω' => "\\omega", 'Α' => "A",
        'Β' => "B", 'Γ' => "\\Gamma", 'Δ' => "\\Delta", 'Ε' => "E",
        'Ζ' => "Z", 'Η' => "H", 'Θ' | 'ϴ' => "\\Theta", 'Ι' => "I",
        'Κ' => "K", 'Λ' => "\\Lambda", 'Μ' => "M", 'Ν' => "N",
        'Ξ' => "\\Xi", 'Ο' => "O", 'Π' => "\\Pi", 'Ρ' => "P",
        'Σ' => "\\Sigma", 'Τ' => "T", 'Υ' => "\\Upsilon", 'Φ' => "\\Phi",
        'Χ' => "X", 'Ψ' => "\\Psi", 'Ω' => "\\Omega", '×' => "\\times",
        '÷' => "\\div", '±' => "\\pm", '∓' => "\\mp", '⋅' | '·' | '∙' => "\\cdot",
        '∗' => "\\ast", '∘' => "\\circ", '∖' => "\\setminus", '⊕' => "\\oplus",
        '⊖' => "\\ominus", '⊗' => "\\otimes", '⊘' => "\\oslash", '⊙' => "\\odot",
        '∪' => "\\cup", '∩' => "\\cap", '⊎' => "\\uplus", '⊓' => "\\sqcap",
        '⊔' => "\\sqcup", '∧' => "\\wedge", '∨' => "\\vee", '†' => "\\dagger",
        '‡' => "\\ddagger", '⋆' => "\\star", '≤' | '⩽' => "\\le", '≥' | '⩾' => "\\ge",
        '≠' => "\\ne", '≈' => "\\approx", '≡' => "\\equiv", '≢' => "\\not\\equiv",
        '≅' => "\\cong", '≃' => "\\simeq", '∼' => "\\sim", '≁' => "\\nsim",
        '∝' => "\\propto", '≪' => "\\ll", '≫' => "\\gg", '≺' => "\\prec",
        '≻' => "\\succ", '⪯' | '≼' => "\\preceq", '⪰' | '≽' => "\\succeq", '⊂' => "\\subset",
        '⊃' => "\\supset", '⊆' => "\\subseteq", '⊇' => "\\supseteq", '⊄' => "\\not\\subset",
        '⊈' => "\\nsubseteq", '⊊' => "\\subsetneq", '⊋' => "\\supsetneq", '⊏' => "\\sqsubset",
        '⊐' => "\\sqsupset", '⊑' => "\\sqsubseteq", '⊒' => "\\sqsupseteq", '∈' => "\\in",
        '∉' => "\\notin", '∋' => "\\ni", '∌' => "\\not\\ni", '≐' => "\\doteq",
        '≜' => "\\triangleq", '≝' => "\\overset{\\mathrm{def}}{=}", '≔' => "\\coloneqq",
        '⊥' => "\\perp", '∥' => "\\parallel", '∦' => "\\nparallel", '⊢' => "\\vdash",
        '⊣' => "\\dashv", '⊨' => "\\models", '⊤' => "\\top", '≍' => "\\asymp",
        '≀' => "\\wr", '⋈' => "\\bowtie", '∣' => "\\mid", '∤' => "\\nmid",
        '→' => "\\to", '←' => "\\leftarrow", '↔' => "\\leftrightarrow", '⇒' => "\\Rightarrow",
        '⇐' => "\\Leftarrow", '⇔' => "\\Leftrightarrow", '↑' => "\\uparrow", '↓' => "\\downarrow",
        '↕' => "\\updownarrow", '⇑' => "\\Uparrow", '⇓' => "\\Downarrow", '⇕' => "\\Updownarrow",
        '↦' => "\\mapsto", '⟶' => "\\longrightarrow", '⟵' => "\\longleftarrow",
        '⟷' => "\\longleftrightarrow", '⟹' => "\\Longrightarrow", '⟸' => "\\Longleftarrow",
        '⟺' => "\\Longleftrightarrow", '⟼' => "\\longmapsto", '↗' => "\\nearrow",
        '↘' => "\\searrow", '↙' => "\\swarrow", '↖' => "\\nwarrow", '↩' => "\\hookleftarrow",
        '↪' => "\\hookrightarrow", '⇀' => "\\rightharpoonup", '↼' => "\\leftharpoonup",
        '⇌' => "\\rightleftharpoons", '↶' => "\\curvearrowleft", '↷' => "\\curvearrowright",
        '∀' => "\\forall", '∃' => "\\exists", '∄' => "\\nexists", '¬' => "\\neg",
        '∅' | '⌀' => "\\emptyset", '∞' => "\\infty", '∂' => "\\partial", '∇' => "\\nabla",
        '∴' => "\\therefore", '∵' => "\\because", 'ℵ' => "\\aleph", 'ℶ' => "\\beth",
        'ℏ' => "\\hbar", 'ℓ' => "\\ell", '℘' => "\\wp", '℧' => "\\mho",
        '√' => "\\surd", '∠' => "\\angle", '∡' => "\\measuredangle", '△' => "\\triangle",
        '□' | '◻' => "\\square", '◊' => "\\lozenge", '°' => "^{\\circ}", '′' => "'",
        '″' => "''", '‴' => "'''", '⁗' => "''''", '…' => "\\ldots",
        '⋯' => "\\cdots", '⋮' => "\\vdots", '⋱' => "\\ddots", '⋰' => "\\iddots",
        '⟨' | '〈' => "\\langle", '⟩' | '〉' => "\\rangle", '⌈' => "\\lceil", '⌉' => "\\rceil",
        '⌊' => "\\lfloor", '⌋' => "\\rfloor", '‖' => "\\Vert", '−' | '‐' | '‑' | '‒' | '–' => "-",
        '\u{2009}' | '\u{200a}' | '\u{2006}' => "\\,", '\u{2005}' | '\u{2004}' => "\\:",
        '\u{2003}' | '\u{2002}' => "\\;",
        '\u{2061}' | '\u{2062}' | '\u{2063}' | '\u{2064}' | '\u{200b}' | '\u{feff}' => "",
        _ => return None,
    }))
}

pub(super) fn delimiter(c: char) -> &'static str {
    match c {
        '(' | '⟮' => "(",
        ')' | '⟯' => ")",
        '[' => "[",
        ']' => "]",
        '{' => "\\{",
        '}' => "\\}",
        '|' | '∣' => "|",
        '‖' | '∥' => "\\Vert",
        '⟨' | '〈' => "\\langle",
        '⟩' | '〉' => "\\rangle",
        '⌈' => "\\lceil",
        '⌉' => "\\rceil",
        '⌊' => "\\lfloor",
        '⌋' => "\\rfloor",
        '/' => "/",
        '\\' => "\\backslash",
        '↑' => "\\uparrow",
        '↓' => "\\downarrow",
        '↕' => "\\updownarrow",
        '⇑' => "\\Uparrow",
        '⇓' => "\\Downarrow",
        '⇕' => "\\Updownarrow",
        _ => ".",
    }
}

pub(super) fn accent(c: char) -> Option<&'static str> {
    Some(match c {
        '\u{300}' | '`' => "\\grave",
        '\u{301}' | '´' => "\\acute",
        '\u{302}' | '^' | 'ˆ' => "\\hat",
        '\u{303}' | '~' | '˜' => "\\tilde",
        '\u{304}' | '¯' | 'ˉ' => "\\bar",
        '\u{305}' | '‾' | '⎴' => "\\overline",
        '\u{306}' | '˘' => "\\breve",
        '\u{307}' | '˙' => "\\dot",
        '\u{308}' | '¨' => "\\ddot",
        '\u{30a}' | '˚' => "\\mathring",
        '\u{30c}' | 'ˇ' => "\\check",
        '\u{332}' | '_' | '⎵' => "\\underline",
        '\u{20d6}' | '←' => "\\overleftarrow",
        '\u{20d7}' | '→' => "\\vec",
        '\u{20e1}' | '↔' => "\\overleftrightarrow",
        '\u{20db}' => "\\dddot",
        '\u{20dc}' => "\\ddddot",
        '\u{20ee}' => "\\underleftarrow",
        '\u{20ef}' => "\\underrightarrow",
        '⏞' | '︷' => "\\overbrace",
        '⏟' | '︸' => "\\underbrace",
        _ => return None,
    })
}

pub(super) fn function_name(name: &str) -> Option<String> {
    const KNOWN: &[&str] = &[
        "arccos", "arcsin", "arctan", "arg", "cos", "cosh", "cot", "coth", "csc", "deg", "det",
        "dim", "exp", "gcd", "hom", "inf", "ker", "lg", "lim", "liminf", "limsup", "ln", "log",
        "max", "min", "Pr", "sec", "sin", "sinh", "sup", "tan", "tanh",
    ];
    let name = name.trim();
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    Some(if KNOWN.contains(&name) {
        format!("\\{name}")
    } else {
        format!("\\operatorname{{{name}}}")
    })
}

#[cfg(test)]
mod tests {
    use super::{Tex, fold_alnum};

    fn written(tex: &str) -> Tex {
        let mut written = Tex::default();
        written.push_str(tex);
        written
    }

    #[test]
    fn a_raise_collides_only_with_a_superscript_it_cannot_merge_with() {
        let rows = [
            ("x^{2}", true),
            ("x^{2} ", true),
            (r"x^{\circ}", true),
            ("x^{2}_{i}", true),
            ("x_{i}^{2}", true),
            ("x'_{i}", true),
            ("x", false),
            ("x'", false),
            ("x''", false),
            ("x_{i}", false),
            ("{x^{2}}", false),
            (r"x\ ", false),
            (r"x^\{", true),
            (r"x^{\}}", true),
            ("", false),
        ];
        for (tex, collides) in rows {
            assert_eq!(written(tex).carries_superscript(), collides, "{tex}");
        }
        let mut open = written("x^{2}");
        open.push_char('{');
        assert!(!open.carries_superscript(), "a group opened after it starts a level of its own");
    }

    #[test]
    fn a_letter_after_a_control_word_is_kept_apart() {
        let rows = [
            (r"x\alpha", "x\\alpha y"),
            (r"\left\langle", "\\left\\langle y"),
            (r"\,", "\\,y"),
            (r"\\", "\\\\y"),
            ("alpha", "alphay"),
            ("", "y"),
        ];
        for (before, after) in rows {
            let mut tex = written(before);
            tex.push_char('y');
            assert_eq!(tex.out, after, "{before}");
        }
    }

    #[test]
    fn alphanumerics_fold_inside_each_block_and_stop_at_its_edges() {
        const FOLDED: &[(&str, char, char, &str)] = &[
            ("a letterlike symbol", 'ℝ', 'R', "\\mathbb"),
            ("an italic letterlike symbol", 'ℎ', 'h', "\\mathit"),
            ("the first Latin letter", '\u{1D400}', 'A', "\\mathbf"),
            ("the first lowercase Latin letter", '\u{1D41A}', 'a', "\\mathbf"),
            ("the last Latin letter", '\u{1D6A3}', 'z', "\\mathtt"),
            ("the first Greek letter", '\u{1D6A8}', '\u{391}', "\\mathbf"),
            ("nabla", '\u{1D6C1}', '∇', "\\mathbf"),
            ("the first lowercase Greek letter", '\u{1D6C2}', 'α', "\\mathbf"),
            ("the first Greek symbol", '\u{1D6DB}', '∂', "\\mathbf"),
            ("the last Greek symbol", '\u{1D7C9}', 'ϖ', "\\mathsf"),
            ("the first digit", '\u{1D7CE}', '0', "\\mathbf"),
            ("the last digit", '\u{1D7FF}', '9', "\\mathtt"),
        ];
        for &(name, c, base, command) in FOLDED {
            let folded = fold_alnum(c).map(|(base, variant)| (base, variant.command()));
            assert_eq!(folded, Some((base, command)), "{name}");
        }
        const KEPT: &[(&str, char)] = &[
            ("a letter outside the blocks", 'x'),
            ("below the blocks", '\u{1D3FF}'),
            ("past the Latin letters", '\u{1D6A4}'),
            ("past the Greek letters", '\u{1D7CA}'),
            ("past the digits", '\u{1D800}'),
        ];
        for &(name, c) in KEPT {
            assert!(fold_alnum(c).is_none(), "{name}");
        }
    }
}
