import bpy

# Delete all existing objects
bpy.ops.object.select_all(action='SELECT')
bpy.ops.object.delete(use_global=False)

# Create a cube at the origin
bpy.ops.mesh.primitive_cube_add(location=(0, 0, 0))

# Save the scene to the specified path
output_path = r"C:/Users/KIRPIC~1/AppData/Local/Temp/blender-probe/out2/GPTOSS20_t0.2_ru_r1/task1.blend"
bpy.ops.wm.save_mainfile(filepath=output_path)