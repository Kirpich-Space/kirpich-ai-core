

import bpy
import os

# Delete pre-existing mesh datablock named Cube if present (so we don't get duplicate meshes)
for m in list(bpy.data.meshes):
    if m.name == "Cube":
        del m

# Remove any objects from collections to start clean
for coll in bpy.data.collections.values():
    coll.objects.clear()

bpy.context.view_layer.objects.active = None  # clear selection

# Create a single cube at the origin (unit size, centered)
bpy.ops.mesh.primitive_cube_add(location=(0.0, 0.0, 0.0))

# Save scene to target path
path = r"C:/Users/KIRPIC~1/AppData/Local/Temp/blender-probe/out/Fable9B/task1.blend"
os.makedirs(os.path.dirname(path), exist_ok=True)
bpy.ops.wm.save_as_mainfile(filepath=path)