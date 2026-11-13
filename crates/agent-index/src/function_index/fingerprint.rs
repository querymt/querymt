//! Structural fingerprinting: SimHash + syn-based Rust fingerprinting

use similarity_core::tree::TreeNode;
use std::rc::Rc;

// ---------------------------------------------------------------------------
// SimHash
// ---------------------------------------------------------------------------

/// Compute a 64-bit SimHash from an iterator of string tokens.
///
/// For each token, every bit in its rapidhash contributes +1 (if set) or -1
/// (if clear) to a per-bit accumulator.  The final fingerprint has bit `i`
/// set iff the accumulator for bit `i` is positive.
///
/// Comparison: `(a ^ b).count_ones()` gives the Hamming distance (0 = identical,
/// 64 = completely different).  Pairs with distance > ~25 are rejected as
/// structurally dissimilar before the expensive TSED step.
pub(crate) fn simhash<I, S>(tokens: I) -> u64
where
    I: Iterator<Item = S>,
    S: AsRef<str>,
{
    let mut v = [0i32; 64];
    for token in tokens {
        let h = rapidhash::v3::rapidhash_v3(token.as_ref().as_bytes());
        for i in 0u64..64 {
            if (h >> i) & 1 == 1 {
                v[i as usize] += 1;
            } else {
                v[i as usize] -= 1;
            }
        }
    }
    let mut fp = 0u64;
    for (i, val) in v.iter().enumerate() {
        if *val > 0 {
            fp |= 1u64 << i;
        }
    }
    fp
}

/// Walk a `TreeNode` tree in DFS pre-order and collect all node-kind labels.
fn collect_node_kinds(node: &Rc<TreeNode>, out: &mut Vec<String>) {
    out.push(node.label.clone());
    for child in &node.children {
        collect_node_kinds(child, out);
    }
}

/// Compute a SimHash fingerprint from the node-kind 3-grams of an AST.
///
/// This is language-agnostic: it works with any `TreeNode` produced by any
/// of the tree-sitter-based parsers (`RustParser`, `PythonParser`,
/// `GenericTreeSitterParser`).  The resulting fingerprint is rename-invariant
/// because identifier names are not part of the node *kind*.
pub(crate) fn structural_simhash_from_tree(root: &Rc<TreeNode>) -> u64 {
    let mut kinds = Vec::new();
    collect_node_kinds(root, &mut kinds);
    if kinds.len() < 3 {
        return 0;
    }
    simhash(
        kinds
            .windows(3)
            .map(|w| format!("{}|{}|{}", w[0], w[1], w[2])),
    )
}

// ---------------------------------------------------------------------------
// syn-based Rust fingerprinting
// ---------------------------------------------------------------------------

/// Structural features extracted from a Rust function body with `syn`.
///
/// Designed to distinguish functions by their *shape* without caring about
/// identifier names.  Two functions with different shapes should have
/// different fingerprints; two structurally similar functions should have
/// similar fingerprints.
struct RustFeatures {
    param_count: u32,
    /// FNV-like hash of joined syntactic parameter-type strings
    param_type_hash: u64,
    /// FNV-like hash of the return-type string (0 if unit / absent)
    return_type_hash: u64,
    if_count: u32,
    match_count: u32,
    loop_count: u32,
    let_count: u32,
    call_count: u32,
    /// SimHash of callee-name 1-grams (names of functions/methods called)
    callee_simhash: u64,
}

impl RustFeatures {
    fn to_fingerprint(&self) -> u64 {
        // Mix all features into a single u64.
        // We xor-fold the independent hashes and mix counts via rapidhash
        // so that even single differences propagate widely.
        let counts_blob: [u8; 28] = {
            let mut b = [0u8; 28];
            b[0..4].copy_from_slice(&self.param_count.to_le_bytes());
            b[4..8].copy_from_slice(&self.if_count.to_le_bytes());
            b[8..12].copy_from_slice(&self.match_count.to_le_bytes());
            b[12..16].copy_from_slice(&self.loop_count.to_le_bytes());
            b[16..20].copy_from_slice(&self.let_count.to_le_bytes());
            b[20..24].copy_from_slice(&self.call_count.to_le_bytes());
            // high nibble of counts blob gets the bucket for return type presence
            b[24..28].copy_from_slice(&((self.return_type_hash != 0) as u32).to_le_bytes());
            b
        };
        let counts_hash = rapidhash::v3::rapidhash_v3(&counts_blob);
        counts_hash
            ^ self.param_type_hash
            ^ self.return_type_hash.rotate_left(17)
            ^ self.callee_simhash.rotate_left(31)
    }
}

/// A `syn::visit::Visit` implementation that counts structural features.
struct RustVisitor {
    if_count: u32,
    match_count: u32,
    loop_count: u32,
    let_count: u32,
    call_count: u32,
    callee_names: Vec<String>,
}

impl RustVisitor {
    fn new() -> Self {
        Self {
            if_count: 0,
            match_count: 0,
            loop_count: 0,
            let_count: 0,
            call_count: 0,
            callee_names: Vec::new(),
        }
    }
}

impl<'ast> syn::visit::Visit<'ast> for RustVisitor {
    fn visit_expr_if(&mut self, node: &'ast syn::ExprIf) {
        self.if_count += 1;
        syn::visit::visit_expr_if(self, node);
    }
    fn visit_expr_match(&mut self, node: &'ast syn::ExprMatch) {
        self.match_count += 1;
        syn::visit::visit_expr_match(self, node);
    }
    fn visit_expr_while(&mut self, node: &'ast syn::ExprWhile) {
        self.loop_count += 1;
        syn::visit::visit_expr_while(self, node);
    }
    fn visit_expr_for_loop(&mut self, node: &'ast syn::ExprForLoop) {
        self.loop_count += 1;
        syn::visit::visit_expr_for_loop(self, node);
    }
    fn visit_expr_loop(&mut self, node: &'ast syn::ExprLoop) {
        self.loop_count += 1;
        syn::visit::visit_expr_loop(self, node);
    }
    fn visit_local(&mut self, node: &'ast syn::Local) {
        self.let_count += 1;
        syn::visit::visit_local(self, node);
    }
    fn visit_expr_call(&mut self, node: &'ast syn::ExprCall) {
        self.call_count += 1;
        // Capture the callee path for SimHash
        if let syn::Expr::Path(p) = node.func.as_ref()
            && let Some(seg) = p.path.segments.last()
        {
            self.callee_names.push(seg.ident.to_string());
        }
        syn::visit::visit_expr_call(self, node);
    }
    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        self.call_count += 1;
        self.callee_names.push(node.method.to_string());
        syn::visit::visit_expr_method_call(self, node);
    }
}

/// Produce a compact structural sketch of a `syn::Type` without relying on
/// `quote`.  We walk the type tree and collect segment idents, joined by
/// spaces.  Examples:
/// - `Option<String>`  → `"Option String"`
/// - `HashMap<K, V>`   → `"HashMap"`  (generic args omitted for brevity)
/// - `&str`            → `"str"`
/// - `fn(u32) -> u64`  → `"fn u32 u64"`
fn type_sketch(ty: &syn::Type) -> String {
    match ty {
        syn::Type::Path(p) => p
            .path
            .segments
            .iter()
            .map(|s| s.ident.to_string())
            .collect::<Vec<_>>()
            .join(" "),
        syn::Type::Reference(r) => type_sketch(&r.elem),
        syn::Type::Slice(s) => format!("slice {}", type_sketch(&s.elem)),
        syn::Type::Array(a) => format!("array {}", type_sketch(&a.elem)),
        syn::Type::Tuple(t) => {
            let parts: Vec<_> = t.elems.iter().map(type_sketch).collect();
            format!("tuple {}", parts.join(" "))
        }
        syn::Type::BareFn(f) => {
            let inputs: Vec<_> = f.inputs.iter().map(|a| type_sketch(&a.ty)).collect();
            let output = match &f.output {
                syn::ReturnType::Default => String::new(),
                syn::ReturnType::Type(_, ty) => type_sketch(ty),
            };
            format!("fn {} {}", inputs.join(" "), output)
        }
        syn::Type::Ptr(p) => type_sketch(&p.elem),
        syn::Type::ImplTrait(_) | syn::Type::TraitObject(_) => "dyn".to_string(),
        _ => String::new(),
    }
}

/// Compute a structural fingerprint for a Rust function from its *full*
/// function text (including signature) using `syn`.
///
/// Falls back to `0` if `syn` cannot parse the source (e.g. incomplete
/// snippets).
pub(crate) fn rust_structural_fingerprint(fn_source: &str) -> u64 {
    // Try parsing as a complete item first; fall back to wrapping in a dummy fn.
    let item_fn: Result<syn::ItemFn, _> = syn::parse_str(fn_source);
    let item_fn = match item_fn {
        Ok(f) => f,
        Err(_) => {
            // Body-only snippet: wrap it
            let wrapped = format!("fn __dummy__() {{ {} }}", fn_source);
            match syn::parse_str::<syn::ItemFn>(&wrapped) {
                Ok(f) => f,
                Err(_) => return 0,
            }
        }
    };

    // --- Parameter features ---
    let param_count = item_fn.sig.inputs.len() as u32;

    // Collect a structural representation of each parameter type.
    // We walk the syn::Type tree and collect ident names — this gives a
    // rename-agnostic sketch of the type structure (e.g. "Option String"
    // for `Option<String>`).
    let param_type_strs: Vec<String> = item_fn
        .sig
        .inputs
        .iter()
        .map(|arg| match arg {
            syn::FnArg::Typed(pat_type) => type_sketch(&pat_type.ty),
            syn::FnArg::Receiver(_) => "self".to_string(),
        })
        .collect();
    let param_type_hash = rapidhash::v3::rapidhash_v3(param_type_strs.join(",").as_bytes());

    // --- Return type ---
    let return_type_hash = match &item_fn.sig.output {
        syn::ReturnType::Default => 0u64,
        syn::ReturnType::Type(_, ty) => rapidhash::v3::rapidhash_v3(type_sketch(ty).as_bytes()),
    };

    // --- Structural counts + callee names via visitor ---
    let mut visitor = RustVisitor::new();
    syn::visit::visit_item_fn(&mut visitor, &item_fn);

    let callee_simhash = if visitor.callee_names.is_empty() {
        0
    } else {
        simhash(visitor.callee_names.iter().map(|s| s.as_str()))
    };

    RustFeatures {
        param_count,
        param_type_hash,
        return_type_hash,
        if_count: visitor.if_count,
        match_count: visitor.match_count,
        loop_count: visitor.loop_count,
        let_count: visitor.let_count,
        call_count: visitor.call_count,
        callee_simhash,
    }
    .to_fingerprint()
}
