import bpy, os

# Remove all existing objects
bpy.ops.object.select_all(action='SELECT')
bpy.ops.object.delete(use_global=False)

# Create a new cube at the world origin
bpy.ops.mesh.primitive_cube_add(location=(0, 0, 0))

# Ensure output directory exists
output_path = r"C:\Users\KIRPIC~1\AppData\Local\Temp\blender-probe\out2\GPTOSS20_t0.7_en_r3\task1.blend"
os.makedirs(os.path.dirname(output_path), exist_ok=True)

# Save the current blend file to the specified path
bpy.ops.wm.save_mainfile(filepath=output_path)