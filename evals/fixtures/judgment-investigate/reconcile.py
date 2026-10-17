import cache as c


def merged(prs):
    return [n for n in prs if c.read_any("pr:%d" % n) == "merged"]
