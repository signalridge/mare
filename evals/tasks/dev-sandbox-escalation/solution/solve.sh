#!/bin/bash
# reference output (Harbor oracle agent): validates the file check only — the escalation checks
# need mare's logs
python3 -c "
import csv, collections
c = collections.Counter(r['status'] for r in csv.DictReader(open('/app/orders.csv')))
open('/srv/reports/order-status.txt', 'w').write(''.join(f'{k} {v}\n' for k, v in sorted(c.items())))
"
