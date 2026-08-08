import bpy
import os

# Clear existing objects in the scene
bpy.ops.object.select_all(action='SELECT')
bpy.ops.object.delete()

# Create a cube at the origin (0, 0, 0)
bpy.ops.mesh.primitive_cube_add(size=2, location=(0, 0, 0))

# Define the target file path
filepath = "C:/Users/KIRPIC~1/AppData/Local/Temp/blender-probe/out2/Gemma27_t0.2_ru_r1/task1.blend"

# Ensure the directory exists before saving
directory = os.path.dirname(filepath)
if not os.path.exists(directory):
    os.makedirs(directory, exist_ok=True)

# Save the scene as a .blend file
bpy.ops.wm.save_as_mainfile(filepath=filepath)