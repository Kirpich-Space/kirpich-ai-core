

import bpy
import os

# Clear existing objects to start from a clean scene
for obj in bpy.data.objects:
    del obj

# Create a unit cube centered at the world origin (0, 0, 0)
bpy.ops.mesh.primitive_cube_add(size=1)

# Ensure the target directory exists
path = r"C:/Users/KIRPIC~1/AppData/Local/Temp/blender-probe/out2/Fable9B_t0.2_en_r3/task1.blend"
os.makedirs(os.path.dirname(path), exist_ok=True)

# Save the current scene to the specified path
bpy.ops.wm.save_as_mainfile(filepath=path)