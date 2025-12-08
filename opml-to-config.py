#!/usr/bin/env python3

import argparse
import toml
import xml.etree.ElementTree as ET


def extract_feed_urls(root):
    urls = []
    seen = set()

    def walk(node):
        # Common OPML feed attributes
        for attr in ("xmlUrl", "xmlurl", "url", "href"):
            if attr in node.attrib:
                u = node.attrib[attr].strip()
                if u and u not in seen:
                    seen.add(u)
                    urls.append(u)
                break
        for child in node.findall("outline"):
            walk(child)

    for outline in root.findall(".//outline"):
        walk(outline)

    return urls


def opml_to_toml(opml_path):
    tree = ET.parse(opml_path)
    root = tree.getroot()

    urls = extract_feed_urls(root)

    data = {"feeds": [{"url": u} for u in urls]}
    return toml.dumps(data)


def main():
    p = argparse.ArgumentParser(description="Convert OPML to TOML (using python-toml)")
    p.add_argument("opml", help="Input OPML file")
    p.add_argument("-o", "--output", help="Output TOML file (default: stdout)")
    args = p.parse_args()

    toml_text = opml_to_toml(args.opml)

    if args.output:
        with open(args.output, "w", encoding="utf-8") as f:
            f.write(toml_text)
    else:
        print(toml_text, end="")


if __name__ == "__main__":
    main()
