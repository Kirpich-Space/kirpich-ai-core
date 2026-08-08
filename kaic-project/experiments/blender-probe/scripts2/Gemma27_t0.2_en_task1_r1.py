import bpy
import os

# Remove all objects from the scene to ensure a clean start
for obj in bpy.data.objects:
    bpy.data.objects.remove(obj, do_unlink=True)

# Create a cube at the world origin (0, 0, 0)
bpy.ops.mesh.primitive_cube_add(size=2, location=(0, 0, 0))

# Define the target file path
filepath = "C:/Users/KIRPIC~1/AppData/Local/Temp/blender-probe/out2/Gemma27_t0.2_en_r1/task1.blend"

# Ensure the directory exists before saving
directory = os.path.dirname(filepath)
if not os.path.exists(directory):
    os.makedirs(directory, exist_ok=True)

# Save the scene as a .blend file
bpy.ops.wm.save_as_mainfile(filepath=filepath)