import bpy
import os

# Clear the default scene
if bpy.context.object and bpy.context.object.mode != 'OBJECT':
    bpy.ops.object.mode_set(mode='OBJECT')
bpy.ops.object.select_all(action='SELECT')
bpy.ops.object.delete()

# Create a cube at the world origin
bpy.ops.mesh.primitive_cube_add(size=2, location=(0, 0, 0))

# Define the save path
save_path = "C:/Users/KIRPIC~1/AppData/Local/Temp/blender-probe/out/Gemma27/task1.blend"

# Create the directory if it does not exist
directory = os.path.dirname(save_path)
if not os.path.exists(directory):
    os.makedirs(directory)

# Save the scene
bpy.ops.wm.save_as_mainfile(filepath=save_path)