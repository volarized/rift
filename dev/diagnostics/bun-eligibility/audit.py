#!/usr/bin/env python3
"""Reduce a bounded ordinary-build Bun dependency observation."""

import argparse
import json
from pathlib import Path
import re

RIFT_SOURCE = 'da38628da2d2613f63fb65f3ffb3eb703ed1d7d9'
RIFT_TREE = '345747e13e159e484c63491e2d09871124635253'
BUN_SOURCE = '744846f844374847c902b5e7fd59b4342a51ef99'
PHASES = {'single': 1, 'single_restore': 1, 'burst': 50, 'burst_restore': 50}
GATES = (
    'capture',
    'omitted_input',
    'framework_context',
    'alias_selector',
    'provider_claim',
    'admission_prefix',
)
MAX_ROWS = 1 + len(PHASES) * (1 + len(GATES))
MAX_BYTES = 256 * 1024


def valid_digest(value):
    return isinstance(value, str) and re.fullmatch(r'[0-9a-f]{64}', value)


def require(condition, message):
    if not condition:
        raise ValueError(message)


def fact_key(row):
    require(row.get('phase') in PHASES, 'unexpected phase')
    family = row.get('family')
    require(family in GATES, 'unexpected gate family')
    require(row.get('status') in {'closed', 'unproved'}, 'unexpected gate status')
    for field in ('observed', 'unknown', 'wall_ns'):
        require(type(row.get(field)) is int and row[field] >= 0, f'invalid {field}')
    maximum = row.get('first')
    if row['status'] == 'closed':
        require(row['unknown'] == 0 and maximum is None, 'closed gate carries unresolved evidence')
    else:
        require(isinstance(maximum, dict) and isinstance(maximum.get('reason'), str) and
                len(maximum['reason'].strip()) > 0, 'unproved gate lacks first witness')
    return row['phase'], family


def read_rows(path):
    require(path.stat().st_size <= MAX_BYTES, 'observer output exceeds byte bound')
    with path.open() as source:
        rows = [json.loads(line) for line in source]
    require(len(rows) <= MAX_ROWS, 'observer output exceeds row bound')
    return rows


def audit(rows, expected_binary):
    require(isinstance(expected_binary, str) and valid_digest(expected_binary), 'invalid expected binary digest')
    require(rows and all(isinstance(row, dict) for row in rows), 'invalid trace objects')
    require(len(rows) <= MAX_ROWS, 'observer output exceeds row bound')
    provenance, *rest = rows
    require(provenance.get('event') == 'provenance', 'missing provenance header')
    require(provenance.get('mode') == 'ordinary_observer', 'trace not from ordinary observation')
    require(provenance.get('rift_source') == RIFT_SOURCE and provenance.get('rift_tree') == RIFT_TREE,
            'Rift source or tree differs')
    require(provenance.get('bun_source') == BUN_SOURCE, 'Bun pin differs')
    require(provenance.get('observer_binary_sha256') == expected_binary, 'observer binary differs')
    total = {}
    snapshots = {}
    for row in rest:
        if row.get('event') == 'gate':
            key = fact_key(row)
            require(key not in total, 'duplicate dependency gate')
            total[key] = row
        elif row.get('event') == 'ordinary':
            phase = row.get('phase')
            require(phase in PHASES and phase not in snapshots, 'unexpected or duplicate ordinary phase')
            snapshots[phase] = row
        else:
            raise ValueError('unexpected trace event')

    phases = {}
    for phase, expected_count in PHASES.items():
        ordinary = snapshots.get(phase)
        oracle_complete = bool(
            ordinary and ordinary.get('source_paths_changed') == expected_count
            and ordinary.get('capture_verified') is True
            and ordinary.get('held_key_check') is True
            and ordinary.get('complete_graph') is True
            and ordinary.get('independent_cold_agrees') is True
            and valid_digest(ordinary.get('ordered_records_and_edges_sha256'))
        )
        checks = []
        for family in GATES:
            row = total.get((phase, family))
            if row is None:
                row = {'family': family, 'status': 'unproved',
                       'first': {'reason': 'required observer coverage is absent'}}
            checks.append({key: value for key, value in row.items() if key not in ('event', 'phase')})
        blockers = [item for item in checks if item['status'] == 'unproved']
        if not oracle_complete:
            blockers.append({'family': 'ordinary_oracle', 'status': 'unproved',
                             'first': {'reason': 'complete current/cold/held oracle comparison is absent'}})
        phases[phase] = {
            'oracle_complete': oracle_complete,
            'first_unproved': blockers[0] if blockers else None,
            'gates': checks,
            'eligible_for_next_shadow': oracle_complete and not blockers,
        }
    return {'rift_source': RIFT_SOURCE, 'rift_tree': RIFT_TREE, 'bun_source': BUN_SOURCE,
            'observer_binary_sha256': expected_binary, 'phases': phases}


def self_test():
    digest = 'a' * 64
    header = {'event': 'provenance', 'mode': 'ordinary_observer', 'rift_source': RIFT_SOURCE,
              'rift_tree': RIFT_TREE, 'bun_source': BUN_SOURCE, 'observer_binary_sha256': digest}
    oracle = [
        {'event': 'ordinary', 'phase': phase, 'source_paths_changed': count, 'capture_verified': True,
         'held_key_check': True, 'complete_graph': True, 'independent_cold_agrees': True,
         'ordered_records_and_edges_sha256': digest}
        for phase, count in PHASES.items()
    ]
    gates = [
        {'event': 'gate', 'phase': phase, 'family': family, 'observed': 12, 'unknown': 0,
         'wall_ns': 100, 'status': 'closed', 'first': None}
        for phase in PHASES for family in GATES
    ]
    rows = [header, *oracle, *gates]
    require(all(x['eligible_for_next_shadow'] for x in audit(rows, digest)['phases'].values()),
            'closed controls declined')
    rows.pop()
    result = audit(rows, digest)['phases']['burst_restore']
    require(not result['eligible_for_next_shadow'] and result['first_unproved']['family'] == 'admission_prefix',
            'absent observation was accepted')
    alias = next(row for row in gates if row['phase'] == 'single' and row['family'] == 'alias_selector')
    alias.update(status='unproved', unknown=1, first={'path': 'index.ts', 'reason': 'untracked negative export'})
    result = audit(rows, digest)['phases']['single']
    require(result['first_unproved']['first']['path'] == 'index.ts', 'actual blocker was not retained')
    try:
        audit(rows + [alias], digest)
    except ValueError as exc:
        require('duplicate' in str(exc), 'unexpected duplicate refusal')
    else:
        raise ValueError('duplicate gate accepted')
    try:
        audit(rows, 'b' * 64)
    except ValueError as exc:
        require('binary differs' in str(exc), 'wrong binary did not refuse')
    else:
        raise ValueError('wrong binary accepted')
    print('5 synthetic reducer checks passed')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('trace', type=Path, nargs='?')
    parser.add_argument('--binary-sha256')
    parser.add_argument('--self-test', action='store_true')
    args = parser.parse_args()
    if args.self_test:
        self_test()
        return
    require(args.trace is not None, 'trace path is required')
    print(json.dumps(audit(read_rows(args.trace), args.binary_sha256), indent=2))


if __name__ == '__main__':
    main()
