import bpy
import os

# Remove all existing objects to ensure a clean scene
for obj in bpy.data.objects:
    bpy.data.objects.remove(obj, do_unlink=True)

# Create a cube at the origin (0, 0, 0)
bpy.ops.mesh.primitive_cube_add(location=(0, 0, 0))

# Define the target file path
filepath = "C:/Users/KIRPIC~1/AppData/Local/Temp/blender-probe/out2/Gemma27_t0.2_ru_r2/task1.blend"

# Create the directory if it does not exist
directory = os.path.dirname(filepath)
if not os.path.exists(directory):
    os.makedirs(directory, exist_ok=True)

# Save the scene as a .blend file
bpy.ops.wm.save_as_mainfile(filepath=filepath)