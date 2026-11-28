#!/usr/bin/env python3
"""Compare forkrail's router with Vercel's, request by request.

The test projects in test-apps/routing, routing-fn and routing-legacy are deployed to both: to
Vercel as forkrail-check-<name> (its production alias, <prefix><name>.vercel.app), and to forkrail,
reached at --forkrail with the Host header <name>.<domain> (a local router, or a router's 8080).
Every case below is sent to both, raw (no redirect followed, no path normalised), and the answers
are compared on what the project's rules decide: the status, Location, the headers the rules set,
and the body (JSON answers by value, an external site's echo by its url and args, anything else by
hash). What each platform adds on its own (server, date, etag, cache headers on files, its own
error pages) isn't compared, but platform pages are marked as such.

    tools/routing-check.py [--forkrail http://127.0.0.1:18080] [--domain forkrail.test]
                           [--only routing [--forkrail-host HOST]] [--show-same]

On AWS: --forkrail https://routing-ID.forkrail.com --forkrail-host routing-ID.forkrail.com --only routing

Exit status 1 when anything differs.
"""

import argparse
import hashlib
import http.client
import json
import sys
import urllib.parse

# Headers the test projects' rules set, or that follow from what was served.
RULE_HEADERS = [
    'location', 'refresh', 'content-type', 'x-frame-options', 'x-content-type-options', 'x-file',
    'x-about', 'x-debug', 'x-slug', 'x-fn', 'x-own', 'x-static', 'x-all',
]
# Paths whose cache-control is compared everywhere else is each platform's own choice.
RULE_CACHE_PATHS = ('/assets/',)
# Projects whose every answer's cache-control is compared (their preset sets it).
CACHE_PROJECTS = ('routing-preset', 'routing-slash')

H = dict  # headers
CASES = {
    'routing': [
        ('GET', '/'), ('GET', '/index'), ('GET', '/index.html'), ('GET', '/about'),
        ('GET', '/about.html'), ('GET', '/about/'), ('GET', '/about?x=1'), ('GET', '/About'),
        ('GET', '/docs'), ('GET', '/docs/'), ('GET', '/docs/intro'), ('GET', '/docs/intro.html'),
        ('GET', '/docs/index'), ('GET', '/docs/index.html'), ('GET', '/assets/site.css'),
        ('GET', '/assets/site.css/'), ('GET', '/vercel.json'), ('GET', '/404'), ('GET', '/404.html'),
        ('GET', '/missing'), ('GET', '/missing/deep/'), ('GET', '/README.md'),
        ('GET', '/old-blog/hello'), ('GET', '/old-blog/hello?x=1&y=2'), ('GET', '/OLD-BLOG/hello'),
        ('GET', '/old-blog/h%C3%A9llo'), ('GET', '/old-blog/a%20b'), ('GET', '/old-blog/a+b'),
        ('GET', '/temporary'), ('GET', '/Temporary'), ('GET', '/temporary/'),
        ('GET', '/docs/v1/intro'), ('GET', '/docs/v1/a/b/c'), ('GET', '/docs/v1'),
        ('GET', '/github'), ('GET', '/github?ref=x'),
        ('GET', '/beta'), ('GET', '/beta', H(cookie='beta=1')), ('GET', '/beta', H(cookie='beta=2')),
        ('GET', '/beta', H(cookie='a=b; beta=1')),
        ('GET', '/foo/edit'), ('GET', '/foo/bar/edit'), ('GET', '/docs/x/edit'), ('GET', '/edit'),
        ('GET', '/Case-Test'), ('GET', '/case-test'), ('GET', '/CASE-TEST'),
        ('GET', '/q-redirect'), ('GET', '/q-redirect?x=1'), ('GET', '/q-redirect?from=z'),
        ('GET', '/q-redirect?x=1&x=2'), ('GET', '/q-redirect?x=a%20b'),
        ('GET', '/has-header'), ('GET', '/has-header', H(**{'x-test': 'yes'})),
        ('GET', '/has-header', H(**{'x-test': 'maybe'})), ('GET', '/has-header', H(**{'x-test': 'YES'})),
        ('GET', '/has-query?page=3'), ('GET', '/has-query?page=abc'), ('GET', '/has-query'),
        ('GET', '/has-query?page=3&x=1'),
        ('GET', '/missing-cookie'), ('GET', '/missing-cookie', H(cookie='session=1')),
        ('GET', '/status-303'), ('GET', '/num/12'), ('GET', '/num/1234'), ('GET', '/num/ab'),
        ('GET', '/plus/a/b'), ('GET', '/plus'), ('GET', '/plus/'), ('GET', '/opt'), ('GET', '/opt/x'),
        ('GET', '/ops', H(**{'x-n': '12'})), ('GET', '/ops', H(**{'x-n': '9'})),
        ('GET', '/ops', H(**{'x-n': '10'})), ('GET', '/ops', H(**{'x-n': 'abc'})), ('GET', '/ops'),
        ('GET', '/pre', H(**{'x-p': 'abc'})), ('GET', '/pre?skip=1', H(**{'x-p': 'abc'})),
        ('GET', '/pre', H(**{'x-p': 'xab'})),
        ('GET', '/host-any'),
        ('GET', '/encoded/a%20b'), ('GET', '/encoded/a%2Fb'), ('GET', '/encoded/%E2%9C%93'),
        ('GET', '/encoded/a%3Fb'),
        ('GET', '/blog/hello'), ('GET', '/blog/hello/'), ('GET', '/blog'), ('GET', '/blog/a%20b'),
        ('GET', '/app'), ('GET', '/app/'), ('GET', '/app/x/y'), ('GET', '/app/index'),
        ('GET', '/example/'), ('GET', '/gone'), ('GET', '/r-query/5'), ('GET', '/r-query/5?id=9'),
        ('GET', '/docs-alias/intro'), ('GET', '/docs-alias'), ('GET', '/nowhere'),
        ('GET', '/echo/a/b?x=1'), ('GET', '/echo/a%20b'),
        ('GET', '/?debug'), ('GET', '/about?debug=1'), ('GET', '/temporary?debug=1'),
        ('HEAD', '/'), ('HEAD', '/missing'), ('HEAD', '/old-blog/x'),
        ('POST', '/'), ('POST', '/about'), ('POST', '/missing'), ('POST', '/old-blog/x'),
        ('OPTIONS', '/'), ('PUT', '/about'),
        ('GET', '//about'), ('GET', '/about%2F'), ('GET', '/%61bout'), ('GET', '/docs/../about'),
        ('GET', '/docs/%2e%2e/about'), ('GET', '/assets/site%2Ecss'), ('GET', '/%2e%2e/about'),
        ('GET', '/a//b?x=a+b'), ('GET', '/old-blog//x'), ('POST', '//about'),
        ('GET', '/q-redirect?x=a+b'), ('GET', '/q-redirect?x=%7E&y=%2F&z=%20'), ('GET', '/q-redirect?x&y='),
        ('GET', '/q-redirect?from=z&x=1'), ('GET', '/q-redirect?a=1&from=z&from=y'), ('GET', '/old-blog/x?a=1&a=2'),
        ('GET', '/old-blog/x?y=a+b&z=%7e&w=%2F'), ('GET', '/q-redirect?b=1&a=2&b=3'),
        ('GET', '/qv?k=abc'), ('GET', '/qv?k=ABC'), ('GET', '/qv?K=abc'),
        ('GET', '/cv', H(cookie='c=abc')), ('GET', '/cv', H(cookie='c=ABC')), ('GET', '/cv', H(cookie='C=abc')),
        ('GET', '/eqop', H(**{'x-e': 'abc'})), ('GET', '/eqop', H(**{'x-e': 'ABC'})),
        ('GET', '/incop', H(**{'x-i': 'DEF'})), ('GET', '/incop', H(**{'x-i': 'ghi'})),
        ('GET', '/neqop', H(**{'x-q': 'ABC'})), ('GET', '/neqop', H(**{'x-q': 'xyz'})),
        ('GET', '/sufop', H(**{'x-s': 'XYZ'})), ('GET', '/reop', H(**{'x-r': 'ABC'})), ('GET', '/reop', H(**{'x-r': 'abc'})),
        ('GET', '/pre', H(**{'x-p': 'ABc'})), ('GET', '/hash'), ('GET', '/hash?x=1'),
        ('GET', '/ext-q'), ('GET', '/ext-q?a=2&b=3'),
        ('GET', '/echo-q/5?x=1&id=9&fixed=2'), ('GET', '/rh'), ('GET', '/rh?debug=1'),
        ('GET', '/assets/aasa'), ('GET', '/notes.md'), ('GET', '/notes'), ('GET', '/LICENSE'),
        ('GET', '/docs/README.md'), ('GET', '/docs/README'),
        ('OPTIONS', '/missing'), ('OPTIONS', '/old-blog/x'), ('OPTIONS', '/gone'), ('DELETE', '/'),
        ('POST', '/gone'), ('HEAD', '/about.html'),
    ],
    'routing-fn': [
        ('GET', '/'), ('GET', '/hello.txt'), ('GET', '/hello'), ('GET', '/hello.txt?x=1'),
        ('POST', '/hello.txt'), ('HEAD', '/hello.txt'), ('GET', '/api/main'), ('GET', '/api/main?x=1'),
        ('GET', '/api/main.py'), ('GET', '/api/main/extra'), ('GET', '/api/other'), ('GET', '/api'),
        ('GET', '/api/'), ('GET', '/API/main'),
        ('GET', '/fn/42'), ('GET', '/fn/42?a=1'), ('GET', '/fn/42?item=9'), ('GET', '/fn/42/'),
        ('GET', '/fn/a%20b'), ('GET', '/fn/%E2%9C%93'), ('GET', '/FN/42'),
        ('GET', '/fn2/a/b?c=1'), ('GET', '/fn2'), ('GET', '/named/x/y?z=1'), ('GET', '/named/x/y?a=q'),
        ('GET', '/teapot'), ('GET', '/teapot?x=1'), ('GET', '/fn-old/7'), ('GET', '/fn-old/7?q=1'),
        ('GET', '/anything/else?x=1'), ('POST', '/fn/1'), ('DELETE', '/anything'),
        ('GET', '/requirements.txt'), ('GET', '/vercel.json'), ('GET', '/public/hello.txt'),
        ('GET', '/fn-ct/1'), ('GET', '/fn/42?a=b+c'), ('GET', '/fn/42?z=%7E&y=%2F'), ('GET', '/fn/42?x&y='),
        ('GET', '/fn/42?a=1&a=2'), ('GET', '/api/main?a=b+c&z=%7E'), ('GET', '/fn2/q?x=a+b'),
        ('GET', '//api/main'), ('POST', '//api/main'), ('OPTIONS', '/hello.txt'), ('OPTIONS', '/anything'),
        ('OPTIONS', '/api/main'), ('HEAD', '/anything'),
    ],
    'routing-legacy': [
        ('GET', '/'), ('GET', '/about'), ('GET', '/about.html'), ('GET', '/ABOUT'),
        ('GET', '/Mixed-Case'), ('GET', '/mixed-case'), ('GET', '/legacy-old/x'),
        ('GET', '/legacy-old/x?y=1'), ('GET', '/LEGACY-OLD/x'), ('GET', '/only-post'),
        ('POST', '/only-post'), ('GET', '/needs-header'), ('GET', '/needs-header', H(**{'x-k': '1'})),
        ('GET', '/teapot'), ('GET', '/items/7'), ('GET', '/items/7?a=1'), ('GET', '/items/7?id=1'),
        ('GET', '/check/page'), ('GET', '/check/nothing'), ('GET', '/page'), ('GET', '/page.html'),
        ('GET', '/nothing'), ('GET', '/api/main'), ('GET', '/api/main?x=1'), ('GET', '/api/main.py'),
        ('GET', '/api/other'), ('HEAD', '/items/7'), ('POST', '/about'), ('GET', '/index.html'),
        ('GET', '/index'), ('GET', '/404.html'),
        ('GET', '/items/7?a=b+c'), ('GET', '/alias/about'), ('GET', '/alias/page.html'), ('GET', '/alias/nothing'),
        ('GET', '/ext/a/b?x=1'), ('GET', '/gone/page'), ('GET', '/gone/nothing'), ('GET', '/m/page'),
        ('GET', '/m/nothing'), ('GET', '/page.html?x=1'), ('OPTIONS', '/nothing'), ('OPTIONS', '/about'),
        ('OPTIONS', '/items/7'), ('POST', '/nothing'), ('GET', '//about'),
    ],
    'routing-preset': [
        ('GET', '/'), ('GET', '/deep/link'), ('GET', '/Deep/Link?x=1'), ('GET', '/static/js/main.abc12345.js'),
        ('GET', '/static/missing.js'), ('GET', '/docs/x'), ('GET', '/service-worker.js'), ('GET', '/manifest.json'),
        ('GET', '/index.html'), ('GET', '/build.sh'), ('GET', '/vercel.json'), ('POST', '/deep/link'),
        ('OPTIONS', '/deep/link'), ('HEAD', '/static/js/main.abc12345.js'),
    ],
    'routing-slash': [
        ('GET', '/'), ('GET', '/about'), ('GET', '/about/'), ('GET', '/about.html'), ('GET', '/about.html/'),
        ('GET', '/docs'), ('GET', '/docs/'), ('GET', '/docs/page'), ('GET', '/docs/page/'),
        ('GET', '/docs/page.html'), ('GET', '/docs/page.html/'), ('GET', '/assets/a.css'), ('GET', '/assets/a.css/'),
        ('GET', '/data.json'), ('GET', '/data.json/'), ('GET', '/old/x'), ('GET', '/old/x/'), ('GET', '/r/x'),
        ('GET', '/r/x/'), ('GET', '/missing'), ('GET', '/missing/'), ('GET', '/new/x/'), ('GET', '/?a=1'),
        ('GET', '/about?a=1'), ('GET', '/a.b/c'), ('GET', '/a.b/c/'), ('GET', '/index.html'), ('GET', '/docs/index.html'),
        ('GET', '/README.md'), ('GET', '/vercel.json'), ('POST', '/about/'), ('OPTIONS', '/about/'),
    ],
}

# A slash after a file's name with `trailingSlash` unset: Vercel's filesystem serves the file (found
# 2026-11-16 by C16; 11 of these differed from Vercel before the engine learned it).
for _project, _paths in {
    'routing': ['/about.html/', '/notes.md/', '/docs/intro.html/', '/LICENSE/'],
    'routing-fn': ['/hello.txt/', '/hello.txt/?x=1', '/public/hello.txt/', '/api/main.py/'],
    'routing-legacy': ['/about.html/', '/page.html/', '/index.html/', '/404.html/', '/page/', '/about/'],
    'routing-preset': ['/manifest.json/', '/static/js/main.abc12345.js/', '/index.html/', '/service-worker.js/', '/404.html/'],
    'routing-slash': ['/about.html/', '/assets/a.css/', '/data.json/', '/docs/page.html/', '/index.html/'],
}.items():
    CASES[_project] += [('GET', _path) for _path in _paths if ('GET', _path) not in CASES[_project]]


def fetch(base, host, method, path, headers):
    """One request, sent as given: (status, {name: [values]}, body bytes)."""
    u = urllib.parse.urlsplit(base)
    cls = http.client.HTTPSConnection if u.scheme == 'https' else http.client.HTTPConnection
    conn = cls(u.netloc, timeout=30)
    try:
        conn.putrequest(method, path, skip_host=True, skip_accept_encoding=True)
        conn.putheader('Host', host)
        conn.putheader('User-Agent', 'forkrail-routing-check')
        for k, v in headers.items():
            conn.putheader(k, v)
        if method in ('POST', 'PUT', 'PATCH', 'DELETE'):
            conn.putheader('Content-Length', '0')
        conn.endheaders()
        r = conn.getresponse()
        body = r.read() if method != 'HEAD' else b''
        hs = {}
        for k, v in r.getheaders():
            hs.setdefault(k.lower(), []).append(v)
        return r.status, hs, body
    finally:
        conn.close()


def view(status, hs, body, host, path, project=''):
    """What's compared, with each platform's host written as {host} and its first label {label}."""
    label = host.split('.')[0]

    def norm(s):
        return s.replace(host, '{host}').replace(label, '{label}')

    out = {'status': status}
    for name in RULE_HEADERS:
        if name in hs:
            vals = [norm(v) for v in hs[name]]
            if name == 'content-type':
                vals = [v.replace(' ', '').lower() for v in vals]
            out[name] = ', '.join(vals)
    if (path.startswith(RULE_CACHE_PATHS) or project in CACHE_PROJECTS) and 'cache-control' in hs:
        out['cache-control'] = ', '.join(hs['cache-control'])
    ctype = ', '.join(hs.get('content-type', []))
    if 'json' in ctype:
        try:
            data = json.loads(body)
        except ValueError:
            data = None
        if isinstance(data, dict) and 'url' in data and 'args' in data:  # httpbin's echo
            data = {'url': urllib.parse.urlsplit(data['url']).path + '?' + (urllib.parse.urlsplit(data['url']).query),
                    'args': data['args'], 'method': data.get('method')}
        if data is not None:
            out['json'] = data
            return out
    out['body'] = hashlib.sha256(body).hexdigest()[:12] + f' ({len(body)} B)' if body else ''
    out['_text'] = body[:90].decode('utf-8', 'replace').replace('\n', ' ')
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('--forkrail', default='http://127.0.0.1:18080')
    ap.add_argument('--domain', default='forkrail.test')
    ap.add_argument('--prefix', default='forkrail-check-', help="Vercel projects' name prefix")
    ap.add_argument('--only')
    ap.add_argument('--forkrail-host', help="with --only: the deployment's own host, e.g. routing-1a2b3c4d.forkrail.com, reached at --forkrail")
    ap.add_argument('--show-same', action='store_true')
    args = ap.parse_args()

    total = differ = 0
    for project, cases in CASES.items():
        if args.only and project != args.only:
            continue
        vhost = f'{args.prefix}{project}.vercel.app'
        fhost = args.forkrail_host or f'{project}.{args.domain}'
        print(f'== {project}: https://{vhost} against {args.forkrail} ({fhost})')
        for case in cases:
            method, path = case[0], case[1]
            headers = case[2] if len(case) > 2 else {}
            total += 1
            try:
                v = view(*fetch(f'https://{vhost}', vhost, method, path, headers), vhost, path, project)
            except Exception as e:  # noqa: BLE001
                v = {'error': str(e)}
            try:
                f = view(*fetch(args.forkrail, fhost, method, path, headers), fhost, path, project)
            except Exception as e:  # noqa: BLE001
                f = {'error': str(e)}
            vt, ft = v.pop('_text', ''), f.pop('_text', '')
            keys = [k for k in dict.fromkeys(list(v) + list(f)) if v.get(k) != f.get(k)]
            hdr = ' '.join(f'{k}: {val}' for k, val in headers.items())
            name = f'{method} {path}' + (f'  [{hdr}]' if hdr else '')
            if not keys:
                if args.show_same:
                    print(f'  same  {name}  -> {v["status"]} {v.get("location", "")}')
                continue
            # A body that differs on an error status with no rule's file behind it is a platform page.
            platform_page = keys == ['body'] and v['status'] >= 400 and f['status'] == v['status']
            if not platform_page:
                differ += 1
            print(f'  {"page" if platform_page else "DIFF"}  {name}')
            for k in keys:
                print(f'        {k:22} vercel: {v.get(k)!s:60.200}')
                print(f'        {"":22} forkrail: {f.get(k)!s:58.200}')
            if 'body' in keys:
                print(f'        {"text":22} vercel: {vt!r}')
                print(f'        {"":22} forkrail: {ft!r}')
    print(f'\n{total} requests, {differ} differ')
    return 1 if differ else 0


if __name__ == '__main__':
    sys.exit(main())
