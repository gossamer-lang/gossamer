// Standard-library items written in Gossamer: shell-style quoting,
// statistics and special functions, and complex numbers. Each is ordinary
// source the program is compiled with, so it behaves the same on every tier.

/// `strings::shell_split`, `shell_quote`, and `shell_join`: POSIX shell
/// word splitting and quoting, as Python's `shlex` does it. Splitting honours
/// single quotes (everything literal), double quotes (a backslash escapes
/// `\`, `"`, `$`, `` ` ``, and a newline), and a backslash outside quotes;
/// an unclosed quote or a trailing backslash is an error. Quoting leaves a
/// word of safe characters as it is and single-quotes anything else.
const STRINGS_SHELL_WRAPPERS: &str = r#"
fn __gos_strings_shell_split(text: String) -> Result<Vec<String>, errors::Error> {
    let mut words: Vec<String> = #[]
    let mut word = ""
    let mut in_word = false
    let mut quote = ' '
    let mut escaped = false
    for c in text {
        if escaped {
            escaped = false
            if quote == '"' && c != '\\' && c != '"' && c != '$' && c != '`' && c != '\n' {
                word.push('\\')
            }
            if !(quote == ' ' && c == '\n') { word.push(c) }
            in_word = true
            continue
        }
        if quote == '\'' {
            if c == '\'' { quote = ' ' } else { word.push(c) }
            continue
        }
        if quote == '"' {
            if c == '"' {
                quote = ' '
            } else if c == '\\' {
                escaped = true
            } else {
                word.push(c)
            }
            continue
        }
        if c == '\\' {
            escaped = true
        } else if c == '\'' || c == '"' {
            quote = c
            in_word = true
        } else if c == ' ' || c == '\t' || c == '\n' || c == '\r' {
            if in_word {
                words.push(word)
                word = ""
                in_word = false
            }
        } else {
            word.push(c)
            in_word = true
        }
    }
    if escaped { return Err(errors::new("strings::shell_split: the text ends in an escape")) }
    if quote != ' ' { return Err(errors::new(f"strings::shell_split: no closing {quote}")) }
    if in_word { words.push(word) }
    Ok(words)
}

fn __gos_strings_shell_safe(c: char) -> bool {
    (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') ||
        c == '@' || c == '%' || c == '+' || c == '=' || c == ':' || c == ',' ||
        c == '.' || c == '/' || c == '-' || c == '_'
}

fn __gos_strings_shell_quote(word: String) -> String {
    if word.is_empty() { return "''" }
    let mut safe = true
    for c in word {
        if !__gos_strings_shell_safe(c) { safe = false }
    }
    if safe { return word }
    let mut out = "'"
    for c in word {
        if c == '\'' { out += "'\"'\"'" } else { out.push(c) }
    }
    out += "'"
    out
}

fn __gos_strings_shell_join(words: [String]) -> String {
    let mut out = ""
    let mut first = true
    for word in words {
        if !first { out += " " }
        out += __gos_strings_shell_quote(word)
        first = false
    }
    out
}
"#;

/// `math::mean`, `median`, `variance`, `stdev`, `pvariance`, `pstdev`, and
/// `quantiles`, as Python's `statistics` module defines them, answering
/// `None` where the statistic is undefined (no data; one value for a sample
/// variance). Sums are compensated (Neumaier) and the variances are two-pass,
/// so a large offset does not cost the digits a one-pass formula loses.
const MATH_STATS_WRAPPERS: &str = r"
fn __gos_math_stats_sum(xs: [f64]) -> f64 {
    let mut sum = 0.0
    let mut carry = 0.0
    for x in xs {
        let next = sum + x
        if sum.abs() >= x.abs() {
            carry += (sum - next) + x
        } else {
            carry += (x - next) + sum
        }
        sum = next
    }
    sum + carry
}

fn __gos_math_mean(xs: [f64]) -> Option<f64> {
    if xs.len() == 0 { return None }
    Some(__gos_math_stats_sum(xs) / (xs.len() as f64))
}

fn __gos_math_stats_squares(xs: [f64], mean: f64) -> f64 {
    let mut deviations: Vec<f64> = Vec::with_capacity(xs.len())
    let mut offset: Vec<f64> = Vec::with_capacity(xs.len())
    for x in xs {
        deviations.push((x - mean) * (x - mean))
        offset.push(x - mean)
    }
    let total = __gos_math_stats_sum(deviations)
    let drift = __gos_math_stats_sum(offset)
    total - drift * drift / (xs.len() as f64)
}

fn __gos_math_variance(xs: [f64]) -> Option<f64> {
    if xs.len() < 2 { return None }
    let mean = __gos_math_mean(xs)?
    Some(__gos_math_stats_squares(xs, mean) / ((xs.len() - 1) as f64))
}

fn __gos_math_pvariance(xs: [f64]) -> Option<f64> {
    if xs.len() == 0 { return None }
    let mean = __gos_math_mean(xs)?
    Some(__gos_math_stats_squares(xs, mean) / (xs.len() as f64))
}

fn __gos_math_stdev(xs: [f64]) -> Option<f64> {
    Some(__gos_math_variance(xs)?.sqrt())
}

fn __gos_math_pstdev(xs: [f64]) -> Option<f64> {
    Some(__gos_math_pvariance(xs)?.sqrt())
}

fn __gos_math_stats_sorted(xs: [f64]) -> Vec<f64> {
    let mut sorted: Vec<f64> = Vec::with_capacity(xs.len())
    for x in xs { sorted.push(x) }
    sorted.sort_by(|a, b| if a < b { -1 } else if a > b { 1 } else { 0 })
    sorted
}

fn __gos_math_median(xs: [f64]) -> Option<f64> {
    let n = xs.len()
    if n == 0 { return None }
    let sorted = __gos_math_stats_sorted(xs)
    if n % 2 != 0 { return Some(sorted[n / 2]) }
    Some((sorted[n / 2 - 1] + sorted[n / 2]) / 2.0)
}

fn __gos_math_quantiles(xs: [f64], n: i64 = 4) -> Option<Vec<f64>> {
    if n < 1 || xs.len() < 2 { return None }
    let data = __gos_math_stats_sorted(xs)
    let m = data.len() + 1
    let mut cuts: Vec<f64> = Vec::with_capacity(n - 1)
    let mut i = 1
    while i < n {
        let mut j = i * m / n
        let mut delta = i * m - j * n
        if j < 1 {
            j = 1
            delta = 0
        }
        if j > data.len() - 1 {
            j = data.len() - 1
            delta = n
        }
        let low = data[j - 1]
        let high = data[j]
        cuts.push((low * ((n - delta) as f64) + high * (delta as f64)) / (n as f64))
        i += 1
    }
    Some(cuts)
}
";

/// `math::erf`, `erfc`, `gamma`, `lgamma`, `factorial`, `comb`, and `perm`.
/// `erf` sums its Taylor series below 2 and `erfc` evaluates its continued
/// fraction (modified Lentz) from 2 up, each the side where it keeps its
/// digits; `gamma` and `lgamma` are the Lanczos approximation (g = 7, nine
/// terms) with the reflection formula below one half, and `gamma` of a whole
/// number up to 20 is the exact factorial. The integer functions answer
/// `i64` and panic, as `*` does, where the answer does not fit.
const MATH_SPECIAL_WRAPPERS: &str = r#"
const __GOS_MATH_SQRT_PI: f64 = 1.7724538509055160273
const __GOS_MATH_LANCZOS: [f64; 9] = [
    0.99999999999980993,
    676.5203681218851,
    -1259.1392167224028,
    771.32342877765313,
    -176.61502916214059,
    12.507343278686905,
    -0.13857109526572012,
    0.0000099843695780195716,
    0.00000015056327351493116,
]

fn __gos_math_erf_series(x: f64) -> f64 {
    let mut term = x
    let mut sum = x
    let x2 = x * x
    let mut n = 0.0
    loop {
        n += 1.0
        term = -term * x2 / n
        let add = term / (2.0 * n + 1.0)
        sum += add
        if add.abs() < 0.0000000000000001 * sum.abs() { break }
        if n > 200.0 { break }
    }
    2.0 / __GOS_MATH_SQRT_PI * sum
}

fn __gos_math_erfc_fraction(x: f64) -> f64 {
    let tiny = 0.000000000000000000000000000001
    let mut f = x
    let mut c = x
    let mut d = 0.0
    let mut k = 1.0
    loop {
        let a = k / 2.0
        d = x + a * d
        if d.abs() < tiny { d = tiny }
        c = x + a / c
        if c.abs() < tiny { c = tiny }
        d = 1.0 / d
        let delta = c * d
        f *= delta
        if (delta - 1.0).abs() < 0.0000000000000002 { break }
        k += 1.0
        if k > 500.0 { break }
    }
    (-x * x).exp() / (__GOS_MATH_SQRT_PI * f)
}

fn __gos_math_erf(x: f64) -> f64 {
    if x.is_nan() { return x }
    if x < 0.0 { return -__gos_math_erf(-x) }
    if x < 2.0 { return __gos_math_erf_series(x) }
    if x > 6.0 { return 1.0 }
    1.0 - __gos_math_erfc_fraction(x)
}

fn __gos_math_erfc(x: f64) -> f64 {
    if x.is_nan() { return x }
    if x < 2.0 { return 1.0 - __gos_math_erf(x) }
    if x > 27.0 { return 0.0 }
    __gos_math_erfc_fraction(x)
}

fn __gos_math_lanczos_sum(z: f64) -> f64 {
    let mut a = __GOS_MATH_LANCZOS[0]
    let mut i = 1
    while i < 9 {
        a += __GOS_MATH_LANCZOS[i] / (z + (i as f64))
        i += 1
    }
    a
}

fn __gos_math_gamma(x: f64) -> f64 {
    if x.is_nan() { return x }
    if x == x.floor() && x <= 0.0 { return f64::NAN }
    if x == x.floor() && x >= 1.0 && x <= 21.0 {
        let mut product = 1.0
        let mut k = 2.0
        while k < x {
            product *= k
            k += 1.0
        }
        return product
    }
    if x < 0.5 {
        let pi = 3.14159265358979323846
        return pi / ((pi * x).sin() * __gos_math_gamma(1.0 - x))
    }
    let z = x - 1.0
    let t = z + 7.5
    2.5066282746310005024 * t.pow(z + 0.5) * (-t).exp() * __gos_math_lanczos_sum(z)
}

fn __gos_math_lgamma(x: f64) -> f64 {
    if x.is_nan() { return x }
    if x == x.floor() && x <= 0.0 { return f64::INFINITY }
    if x == 1.0 || x == 2.0 { return 0.0 }
    if x < 0.5 {
        let pi = 3.14159265358979323846
        return (pi / (pi * x).sin().abs()).ln() - __gos_math_lgamma(1.0 - x)
    }
    let z = x - 1.0
    let t = z + 7.5
    0.91893853320467274178 + (z + 0.5) * t.ln() - t + __gos_math_lanczos_sum(z).ln()
}

fn __gos_math_factorial(n: i64) -> i64 {
    if n < 0 { panic(f"math::factorial: {n} is negative") }
    let mut product = 1
    let mut k = 2
    while k <= n {
        product *= k
        k += 1
    }
    product
}

fn __gos_math_perm(n: i64, k: i64) -> i64 {
    if n < 0 || k < 0 { panic(f"math::perm: arguments {n}, {k} must not be negative") }
    if k > n { return 0 }
    let mut product = 1
    let mut i = 0
    while i < k {
        product *= n - i
        i += 1
    }
    product
}

fn __gos_math_comb(n: i64, k: i64) -> i64 {
    if n < 0 || k < 0 { panic(f"math::comb: arguments {n}, {k} must not be negative") }
    if k > n { return 0 }
    let mut take = k
    if n - k < take { take = n - k }
    // `result * (n - i)` is a multiple of `i + 1`; dividing out the common
    // factor first keeps the intermediate product as small as the answer.
    let mut result = 1
    let mut i = 0
    while i < take {
        let mut a = result
        let mut b = i + 1
        while b != 0 {
            let r = a % b
            a = b
            b = r
        }
        result = (result / a) * ((n - i) / ((i + 1) / a))
        i += 1
    }
    result
}
"#;

/// `math::Complex`: a complex number of two `f64` parts, with the
/// arithmetic operators, `abs` (the modulus), `arg`, `conj`, `exp`, `ln`,
/// `sqrt`, `powf`, and `from_polar`. It prints as `re+imi` (`1+2i`,
/// `0.5-1.5i`).
const MATH_COMPLEX_WRAPPERS: &str = r#"
#[derive(PartialEq)]
struct __gos_math_Complex {
    re: f64
    im: f64

    fn new(re: f64, im: f64) -> __gos_math_Complex { __gos_math_Complex { re: re, im: im } }

    fn from_polar(r: f64, theta: f64) -> __gos_math_Complex {
        __gos_math_Complex { re: r * theta.cos(), im: r * theta.sin() }
    }

    fn abs(&self) -> f64 { self.re.hypot(self.im) }

    fn arg(&self) -> f64 { self.im.atan2(self.re) }

    fn conj(&self) -> __gos_math_Complex { __gos_math_Complex { re: self.re, im: -self.im } }

    fn exp(&self) -> __gos_math_Complex {
        let r = self.re.exp()
        __gos_math_Complex { re: r * self.im.cos(), im: r * self.im.sin() }
    }

    fn ln(&self) -> __gos_math_Complex {
        __gos_math_Complex { re: self.abs().ln(), im: self.arg() }
    }

    fn sqrt(&self) -> __gos_math_Complex {
        let r = self.abs()
        let re = ((r + self.re) / 2.0).sqrt()
        let mut im = ((r - self.re) / 2.0).sqrt()
        if self.im < 0.0 { im = -im }
        __gos_math_Complex { re: re, im: im }
    }

    fn powf(&self, exponent: f64) -> __gos_math_Complex {
        if self.re == 0.0 && self.im == 0.0 { return __gos_math_Complex { re: 0.0, im: 0.0 } }
        __gos_math_Complex::from_polar(self.abs().pow(exponent), self.arg() * exponent)
    }
}

impl Add for __gos_math_Complex {
    fn add(self, o: __gos_math_Complex) -> __gos_math_Complex {
        __gos_math_Complex { re: self.re + o.re, im: self.im + o.im }
    }
}

impl Sub for __gos_math_Complex {
    fn sub(self, o: __gos_math_Complex) -> __gos_math_Complex {
        __gos_math_Complex { re: self.re - o.re, im: self.im - o.im }
    }
}

impl Mul for __gos_math_Complex {
    fn mul(self, o: __gos_math_Complex) -> __gos_math_Complex {
        __gos_math_Complex { re: self.re * o.re - self.im * o.im, im: self.re * o.im + self.im * o.re }
    }
}

impl Div for __gos_math_Complex {
    fn div(self, o: __gos_math_Complex) -> __gos_math_Complex {
        let d = o.re * o.re + o.im * o.im
        __gos_math_Complex {
            re: (self.re * o.re + self.im * o.im) / d,
            im: (self.im * o.re - self.re * o.im) / d,
        }
    }
}

impl Neg for __gos_math_Complex {
    fn neg(self) -> __gos_math_Complex { __gos_math_Complex { re: -self.re, im: -self.im } }
}

impl Display for __gos_math_Complex {
    fn fmt(&self) -> String {
        if self.im < 0.0 || (self.im == 0.0 && (1.0 / self.im) < 0.0) {
            return f"{self.re}-{-self.im}i"
        }
        f"{self.re}+{self.im}i"
    }
}
"#;

/// `math::rand` helpers over an `Rng`: `range` and `range_f64` (half-open,
/// unbiased: a draw past the largest multiple of the span is drawn again),
/// `chance`, `choose`, `sample` (without replacement), `shuffle`,
/// `weighted` (an index drawn by weight), `normal` (Box-Muller), and
/// `exponential`. Each reads the generator's own stream, so a seed gives the
/// same values on every tier. `from_os()` seeds a generator from the host's
/// random source.
const MATH_RAND_WRAPPERS: &str = r#"
fn __gos_rand_below(rng: rand::Rng, span: u64) -> u64 {
    let limit = 18446744073709551615 - (18446744073709551615 % span + 1) % span
    loop {
        let x = rng.next_u64()
        if x <= limit { return x % span }
    }
}

fn __gos_rand_range(rng: rand::Rng, low: i64, high: i64) -> i64 {
    if high <= low { panic(f"rand::range: the range {low}..{high} is empty") }
    let span = (high as% u64) -% (low as% u64)
    ((low as% u64) +% __gos_rand_below(rng, span)) as% i64
}

fn __gos_rand_range_f64(rng: rand::Rng, low: f64, high: f64) -> f64 {
    if !(high > low) { panic(f"rand::range_f64: the range {low}..{high} is empty") }
    let x = low + (high - low) * rng.next_f64()
    if x < high { x } else { low }
}

fn __gos_rand_chance(rng: rand::Rng, p: f64) -> bool {
    rng.next_f64() < p
}

fn __gos_rand_choose<T>(rng: rand::Rng, xs: [T]) -> Option<T> {
    if xs.len() == 0 { return None }
    Some(xs[__gos_rand_range(rng, 0, xs.len())])
}

fn __gos_rand_shuffle<T>(rng: rand::Rng, xs: &mut Vec<T>) {
    let mut i = xs.len() - 1
    while i > 0 {
        let j = __gos_rand_range(rng, 0, i + 1)
        let held = xs[i]
        xs[i] = xs[j]
        xs[j] = held
        i -= 1
    }
}

fn __gos_rand_sample<T>(rng: rand::Rng, xs: [T], k: i64) -> Vec<T> {
    if k < 0 || k > xs.len() { panic(f"rand::sample: {k} of {xs.len()} values") }
    let mut pool: Vec<T> = Vec::with_capacity(xs.len())
    for x in xs { pool.push(x) }
    let mut i = 0
    while i < k {
        let j = __gos_rand_range(rng, i, pool.len())
        let held = pool[i]
        pool[i] = pool[j]
        pool[j] = held
        i += 1
    }
    let mut picked: Vec<T> = Vec::with_capacity(k)
    let mut n = 0
    while n < k {
        picked.push(pool[n])
        n += 1
    }
    picked
}

fn __gos_rand_weighted(rng: rand::Rng, weights: [f64]) -> Option<i64> {
    let mut total = 0.0
    for w in weights {
        if w < 0.0 || w.is_nan() { return None }
        total += w
    }
    if !(total > 0.0) { return None }
    let target = rng.next_f64() * total
    let mut running = 0.0
    let mut i = 0
    let mut last = 0
    for w in weights {
        if w > 0.0 { last = i }
        running += w
        if target < running { return Some(i) }
        i += 1
    }
    Some(last)
}

fn __gos_rand_normal(rng: rand::Rng, mean: f64, stdev: f64) -> f64 {
    let mut u = rng.next_f64()
    while u == 0.0 { u = rng.next_f64() }
    let v = rng.next_f64()
    let z = (-2.0 * u.ln()).sqrt() * (6.283185307179586 * v).cos()
    mean + stdev * z
}

fn __gos_rand_exponential(rng: rand::Rng, rate: f64) -> f64 {
    if !(rate > 0.0) { panic(f"rand::exponential: the rate {rate} is not positive") }
    let mut u = rng.next_f64()
    while u == 0.0 { u = rng.next_f64() }
    -u.ln() / rate
}

fn __gos_rand_from_os() -> Result<rand::Rng, errors::Error> {
    let bytes = crypto::rand::bytes(8)?
    let mut seed = 0
    for b in bytes { seed = (seed <<% 8) | (b as i64) }
    Ok(rand::Rng::new(seed))
}
"#;
