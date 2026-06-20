#!/usr/bin/env python3
"""Bevy 0.18->0.19 mechanical text-API migration.

Wraps `font_size: <f32 expr>` -> `font_size: FontSize::Px(<expr>)` and
`font: <Handle<Font> expr>` -> `font: (<expr>).into()` in Bevy struct literals.

Safe because:
  * `font_size: f32` (fn params / field decls) is left alone (value == "f32").
  * `.into()` on a `Handle<Font>` whose target field is `Handle<Font>` is the
    reflexive `From<T> for T`, i.e. a no-op, so over-application still compiles.
Over-wrapped custom-struct `font_size` literals surface as `expected f32, found
FontSize` and get reverted by hand. The compiler is the safety net.
"""
import re, sys, pathlib

FONT_SIZE = re.compile(r'^(\s*)font_size:\s*(.+?)(,?)\s*$')
FONT = re.compile(r'^(\s*)font:\s*(.+?)(,?)\s*$')

# value prefixes that indicate a TYPE annotation / decl, not a value expr
TYPE_HINT = re.compile(r'^(Option<|Handle<|&|f32|Cow<|Box<|FontSource\b|Vec<)')


def migrate(text: str) -> tuple[str, int, int]:
    out, n_fs, n_f = [], 0, 0
    for line in text.split('\n'):
        m = FONT_SIZE.match(line)
        if m:
            indent, val, comma = m.group(1), m.group(2).strip(), m.group(3)
            if (val != 'f32' and 'FontSize' not in val
                    and not val.endswith('{') and '=>' not in val
                    and not val.startswith('f32')):
                line = f'{indent}font_size: FontSize::Px({val}){comma}'
                n_fs += 1
            out.append(line)
            continue
        m = FONT.match(line)
        if m:
            indent, val, comma = m.group(1), m.group(2).strip(), m.group(3)
            if (not TYPE_HINT.match(val) and '.into()' not in val
                    and 'FontSource' not in val and not val.endswith('{')
                    and '=>' not in val and 'Handle<' not in val):
                line = f'{indent}font: ({val}).into(){comma}'
                n_f += 1
            out.append(line)
            continue
        out.append(line)
    return '\n'.join(out), n_fs, n_f


def main(paths):
    tot_fs = tot_f = 0
    for p in paths:
        p = pathlib.Path(p)
        src = p.read_text()
        new, fs, f = migrate(src)
        if new != src:
            p.write_text(new)
            tot_fs += fs
            tot_f += f
            print(f'{p}: font_size+{fs} font+{f}')
    print(f'TOTAL: font_size {tot_fs}, font {tot_f}')


if __name__ == '__main__':
    main(sys.argv[1:])
