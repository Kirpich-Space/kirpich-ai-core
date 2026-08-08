import bpy
# Delete all existing objects in the scene
bpy.ops.object.select_all(action='SELECT')
bpy.ops.object.delete(use_global=False, confirm=False)

# Create a new cube at world origin
bpy.ops.mesh.primitive_cube_add(location=(0, 0, 0))

# Save the current blend file to the specified path
output_path = r"C:/Users/KIRPIC~1/AppData/Local/Temp/blender-probe/out2/GPTOSS20_t0.0_en_r3/task1.blend"
bpy.ops.wm.save_as_mainfile(filepath=output_path, copy=False)