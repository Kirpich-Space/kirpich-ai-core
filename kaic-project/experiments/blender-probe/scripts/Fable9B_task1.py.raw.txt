

import bpy
from bpy.modules.bmesh import bmesh
import os

# Remove pre-existing meshes named Cube (they could conflict with the new geometry)
for m in bpy.data.meshes:
    if m.name == "Cube":
        del m

# Clear all objects from collections to start clean
for coll in bpy.data.collections.values():
    coll.objects.clear()
bpy.context.view_layer.objects.active = None  # clear selection

# Create cube geometry via bmesh (unit axis-aligned cube centered at origin)
bm = bmesh.new()
for x in (-0.5, 0.5):
    for y in (-0.5, 0.5):
        bm.vertices.add((x, y, 0))      # bottom face vertices (indices 0..3)
        bm.vertices.add((x, y, 1))      # top face vertices (indices 4..7)

for v0, v1, v2, v3 in ((0, 1, 2, 3), (4, 5, 6, 7)):
    bm.faces.add([v0, v1, v2, v3])     # CCW ordering gives outward normals

# Write mesh data block to Blender's datablock
mesh = bpy.data.meshes.new("Cube")
mesh.from_pydata(bm.vertices, bm.edges, bm.faces)
mesh.update()

# Create object from the mesh data block (primitive type will be "MESH")
obj = bpy.data.objects.new("Cube", mesh)
obj.location = (0.0, 0.0, 0.0)

# Save scene to target path
path = r"C:/Users/KIRPIC~1/AppData/Local/Temp/blender-probe/out/Fable9B/task1.blend"
os.makedirs(os.path.dirname(path), exist_ok=True)
bpy.ops.wm.save_as_mainfile(filepath=path)