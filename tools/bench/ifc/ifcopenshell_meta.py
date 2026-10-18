"""Checks what `stream3dm ifc --products` resolves for each product (storey, type, material, property
set and quantity values) against IfcOpenShell's utilities (ifcopenshell.util.element), the
reference, and prints the agreement per file as JSON lines.

    python3 -I tools/bench/ifc/ifcopenshell_meta.py <ifcopenshell dir> <stream3dm binary> <file.ifc>...
"""
import json
import subprocess
import sys
import types

sys.path.insert(0, sys.argv[1])
# The element utilities import shapely for geometry helpers this script never calls.
for name in ("shapely", "shapely.ops"):
    sys.modules.setdefault(name, types.ModuleType(name))
import ifcopenshell  # noqa: E402
import ifcopenshell.util.element as element  # noqa: E402

binary = sys.argv[2]


def storey_of(product):
    """The building storey a product stands on, through containment and aggregation."""
    current = product
    for _ in range(32):
        if current is None:
            return None
        if current.is_a("IfcBuildingStorey"):
            return current
        container = element.get_container(current)
        current = container if container is not None else element.get_aggregate(current)
    return None


def text(value):
    """A property value as stream3dm writes it."""
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, float):
        if value == int(value) and abs(value) < 1e15:
            return str(int(value))
        return f"{value:.6f}".rstrip("0").rstrip(".")
    if isinstance(value, (list, tuple)):
        return ", ".join(text(v) for v in value)
    return str(value)


for path in sys.argv[3:]:
    ours = {}
    out = subprocess.run([binary, "ifc", path, "--products"], capture_output=True, text=True, check=True).stdout
    for line in out.splitlines():
        p = json.loads(line)
        ours[p["id"]] = p
    f = ifcopenshell.open(path)
    stats = {"file": path.split("/")[-1], "products": 0, "storey": [0, 0], "type": [0, 0], "material": [0, 0], "values": [0, 0], "truncated": 0, "examples": []}
    for product in f.by_type("IfcProduct"):
        if not getattr(product, "Representation", None):
            continue
        mine = ours.get(product.id())
        if mine is None:
            stats["examples"].append(f"#{product.id()} {product.is_a()} not listed")
            continue
        stats["products"] += 1
        strings = dict(mine["user_strings"])
        s = storey_of(product)
        expected = (s.Name or f"Storey #{s.id()}") if s is not None else None
        stats["storey"][1] += 1
        if strings.get("Storey") == expected:
            stats["storey"][0] += 1
        elif len(stats["examples"]) < 8:
            stats["examples"].append(f"#{product.id()} storey {strings.get('Storey')!r} vs {expected!r}")
        t = element.get_type(product)
        stats["type"][1] += 1
        if mine.get("type_name") == (t.Name if t is not None and t.Name else None):
            stats["type"][0] += 1
        elif len(stats["examples"]) < 8:
            stats["examples"].append(f"#{product.id()} type {mine.get('type_name')!r} vs {t.Name if t else None!r}")
        materials = [m.Name for m in element.get_materials(product) if getattr(m, "Name", None)]
        stats["material"][1] += 1
        if (mine.get("material") in materials) if materials else mine.get("material") is None:
            stats["material"][0] += 1
        elif len(stats["examples"]) < 8:
            stats["examples"].append(f"#{product.id()} material {mine.get('material')!r} vs {materials}")
        truncated = len(mine["user_strings"]) >= 1000
        for pset, props in element.get_psets(product).items():
            pset = pset if pset not in (None, "None", "") else "Properties"  # unnamed sets
            for name, value in props.items():
                if name == "id" or value is None:
                    continue
                stats["values"][1] += 1
                if strings.get(f"{pset}.{name}") == text(value):
                    stats["values"][0] += 1
                elif truncated:
                    stats["truncated"] += 1
                elif len(stats["examples"]) < 30:
                    stats["examples"].append(f"#{product.id()} {pset}.{name} {strings.get(f'{pset}.{name}')!r} vs {text(value)!r}")
    print(json.dumps(stats), flush=True)
