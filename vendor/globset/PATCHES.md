# Local Patch

This directory vendors `globset` 0.4.18 under its upstream `Unlicense OR MIT`
terms. The workspace patches crates.io to this copy.

`RequiredExtension` patterns previously compiled one `regex-automata::meta::Regex`
per rule. Each regex owns independently bounded NFA and cache state, which makes
the aggregate allocation bound proportional to untrusted ignore-rule count.

The local patch routes those patterns through the existing `RegexSetStrategy`.
It preserves the original pattern-to-global-index mapping and the final sorted
match order while bounding compiled regex and cache state to one shared regex
set. The shared set also disables capture slots because matching only consumes
pattern IDs; this prevents the first PikeVM cache from allocating capture
storage outside the caller-owned ignore-policy budget.
