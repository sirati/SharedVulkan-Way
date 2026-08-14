#!/usr/bin/env bash
# wc-xref.sh -- do the design-document citations in this tree resolve?
#
#   scripts/wc-xref.sh            check, and exit non-zero if any citation is broken
#   scripts/wc-xref.sh --list     print every broken citation, grouped, and stop
#   scripts/wc-xref.sh --sections print each document's section index
#
# WHY THIS EXISTS RATHER THAN A ROUND OF FIXES
#
# This tree was unified from two repositories and has two design documents:
# `docs/design.md` is the Vulkan side and `docs/waycompose-design.md` is the
# Wayland side. Rename detection has repeatedly landed sections in the wrong
# file, and 113 comments were found citing the wrong one.
#
# **A citation naming the wrong document is worse than no citation at all**,
# because it looks checkable. `§14.7` in the right document is an argument you
# can go and read; the same string pointing at the other document resolves to
# something unrelated, and the reader either follows it and is confused or
# trusts it and is misled. §35.10 is the rule — a comment nothing asserts is a
# claim that outlives its truth — and a citation is the one kind of comment that
# *can* be mechanically asserted. So it is, here.
#
# Fixing the 113 without this check would leave the tree one bad merge from
# having them back. The check is the deliverable; the fixes are its output.
#
# WHAT COUNTS AS BROKEN, in decreasing order of how wrong it is
#
#   missing     the document is named and the section does not exist in it.
#               Unambiguously a defect: the reference cannot be followed.
#   ambiguous   the section number exists *twice* in the named document, so the
#               citation resolves to two different places. `docs/design.md` has
#               ten such numbers today (0.15, 0.20, and 1 through 8), which is
#               the section-numbering collision seen from the reader's end.
#   unqualified `design.md §N` with no directory. In a tree with two design
#               documents whose names both end in `design.md`, the bare form
#               names neither -- and it is by far the most common form here, so
#               it is reported separately rather than drowning the rest.
#
# A bare `§N` with no document at all is **not** checked. It means "this file's
# own document", that convention is used ~2271 times, and mechanising it would
# need a file-to-document map that does not exist. Named in the report as a
# known gap rather than silently skipped.
set -uo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT" || exit 2

VULKAN=docs/design.md
WAYLAND=docs/waycompose-design.md
MODE="${1:-check}"

for d in "$VULKAN" "$WAYLAND"; do
  [ -f "$d" ] || { echo "wc-xref: no $d in $ROOT" >&2; exit 2; }
done

# ---------------------------------------------------------------- sections ---
# Every section number a document defines, one per line, with a count.
#
# Headings are `## 14. Title` and `### 14.7 Title`; the number is the first
# token. `-a` throughout: these documents are UTF-8 and a stray byte would make
# grep treat one as binary and silently report nothing, which is §44.7's trap
# and would turn this whole check into a vacuous one.
sections_of() { grep -aoE "^#{1,4} [0-9]+(\.[0-9]+)*" "$1" | awk '{print $2}'; }

index_file() { echo "/tmp/wc-xref-$$-$(echo "$1" | tr / _).idx"; }
build_index() { sections_of "$1" | sort | uniq -c | awk '{print $2, $1}' > "$(index_file "$1")"; }
build_index "$VULKAN"
build_index "$WAYLAND"
trap 'rm -f /tmp/wc-xref-'"$$"'-*.idx' EXIT

# How many times a document defines a section: 0 (missing), 1 (fine), 2+ (ambiguous).
defines() {
  awk -v s="$2" '$1 == s { print $2; found = 1 } END { if (!found) print 0 }' \
    "$(index_file "$1")" | head -1
}

if [ "$MODE" = --sections ]; then
  for d in "$VULKAN" "$WAYLAND"; do
    echo "== $d: $(sections_of "$d" | wc -l) headings, $(sections_of "$d" | sort -u | wc -l) distinct"
    dup=$(sections_of "$d" | sort | uniq -d | tr '\n' ' ')
    [ -n "$dup" ] && echo "   DUPLICATE section numbers: $dup"
  done
  exit 0
fi

# --------------------------------------------------------------- citations ---
# `<doc>.md §N` in any source or harness file. The `§` may be absent in a few
# older comments, which the second alternative catches.
#
# Deliberately not restricted to comment syntax: a citation inside a string
# literal that a program prints is a citation a person will read and follow, and
# it is wrong in exactly the same way.
scan() {
  grep -raoEn "[A-Za-z/-]*design\.md [§ ]?§?[0-9]+(\.[0-9]+)*" \
    --include=*.rs --include=*.sh --include=*.md --include=*.c \
    crates/ scripts/ docs/ 2>/dev/null
}

missing=0; ambig=0; unqual=0; ok_n=0
: > /tmp/wc-xref-$$-missing.txt
: > /tmp/wc-xref-$$-ambig.txt
: > /tmp/wc-xref-$$-unqual.txt

while IFS= read -r hit; do
  loc=${hit%%:*}; rest=${hit#*:}
  lineno=${rest%%:*}; cite=${rest#*:}
  doc=$(printf '%s' "$cite" | grep -oE "^[A-Za-z/-]*design\.md")
  sec=$(printf '%s' "$cite" | grep -oE "[0-9]+(\.[0-9]+)*$")
  [ -n "$sec" ] || continue

  case "$doc" in
    docs/waycompose-design.md) target=$WAYLAND ;;
    docs/design.md)            target=$VULKAN ;;
    waycompose-design.md)      target=$WAYLAND ;;   # unambiguous name, no dir
    design.md)
      # The bare form. It names neither document, so it is reported as
      # unqualified -- but if the section exists in exactly one of the two, that
      # is said too, because it is what makes the fix mechanical.
      inv=$(defines "$VULKAN" "$sec"); inw=$(defines "$WAYLAND" "$sec")
      where=""
      [ "$inv" != 0 ] && where="$where vulkan($inv)"
      [ "$inw" != 0 ] && where="$where wayland($inw)"
      echo "$loc:$lineno: design.md §$sec -> resolves in:${where:- NEITHER}" \
        >> /tmp/wc-xref-$$-unqual.txt
      unqual=$((unqual + 1))
      continue ;;
    *) continue ;;
  esac

  n=$(defines "$target" "$sec")
  if [ "$n" = 0 ]; then
    echo "$loc:$lineno: $doc §$sec -> NO SUCH SECTION" >> /tmp/wc-xref-$$-missing.txt
    missing=$((missing + 1))
  elif [ "$n" -gt 1 ]; then
    echo "$loc:$lineno: $doc §$sec -> defined $n times" >> /tmp/wc-xref-$$-ambig.txt
    ambig=$((ambig + 1))
  else
    ok_n=$((ok_n + 1))
  fi
done < <(scan)

echo "== design-document cross-references =="
echo "  $ok_n resolve to exactly one section"
echo "  $missing name a document and a section that does not exist in it"
echo "  $ambig name a section their document defines more than once"
echo "  $unqual say \`design.md\` with no directory, which names neither document"
echo

# **No truncation for the failing categories, and that is not cosmetic.** The
# first version of this capped the list at 40 lines, and the first use of it —
# driving the fixes from its own output — therefore repaired 41 of 49 and
# reported success on the rest until a re-run found them. A check that hides
# part of what it found is the same defect as one that cannot fail: the number
# in the summary was right and the list under it was not, so anything working
# from the list silently did less than the summary claimed. Only the
# *unqualified* list, which is hundreds long and is not a failure, is capped.
show() {
  [ -s "$2" ] || return 0
  echo "-- $1"
  if [ -n "${3:-}" ]; then sed 's/^/   /' "$2" | head -"$3"
    local n; n=$(wc -l <"$2")
    [ "$n" -gt "$3" ] && echo "   … and $((n - $3)) more (--list shows all)"
  else
    sed 's/^/   /' "$2"
  fi
  echo
}
show "MISSING: the citation cannot be followed" /tmp/wc-xref-$$-missing.txt
show "AMBIGUOUS: the citation resolves to two places" /tmp/wc-xref-$$-ambig.txt
if [ "$MODE" = --list ]; then
  show "UNQUALIFIED: \`design.md\` names neither document" /tmp/wc-xref-$$-unqual.txt 400
fi

# A bare `§N` with no document is the tree's most common form and is not
# checkable here; said out loud so the number above is not mistaken for
# "every citation in the tree".
echo "  not checked: $(grep -raoE "§[0-9]+" --include=*.rs --include=*.sh crates/ scripts/ 2>/dev/null | wc -l) bare \`§N\` references"
echo "  (they mean 'this file's own document'; resolving them needs a file-to-document map)"

rm -f /tmp/wc-xref-$$-missing.txt /tmp/wc-xref-$$-ambig.txt /tmp/wc-xref-$$-unqual.txt

# **Only `missing` fails the check today.** Ambiguity and the unqualified form
# are real and are reported, but there are hundreds of the latter and failing on
# them would make this unrunnable rather than useful -- and a check nobody can
# pass gets disabled, which is §42's other defect. Tighten as they come down.
[ "$missing" -eq 0 ]
