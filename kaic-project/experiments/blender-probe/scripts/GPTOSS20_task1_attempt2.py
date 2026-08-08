import bpy

# Delete all existing objects
bpy.ops.object.select_all(action='SELECT')
bpy.ops.object.delete(use_global=False)

# Add a cube at the world origin
bpy.ops.mesh.primitive_cube_add(size=2, location=(0, 0, 0))

# Save the scene to the specified path
output_path = r"C:/Users/KIRPIC~1/AppData/Local/Temp/blender-probe/out/GPTOSS20/task1.blend"
bpy.ops.wm.save_as_mainfile(filepath=output_path)