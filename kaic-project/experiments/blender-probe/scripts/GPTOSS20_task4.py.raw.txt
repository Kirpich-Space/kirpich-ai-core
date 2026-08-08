import bpy

# Open the specified .blend file
bpy.ops.wm.open_mainfile(filepath=r"C:/Users/KIRPIC~1/AppData/Local/Temp/blender-probe/out/GPTOSS20/task1.blend")

# Ensure the object named 'Cube' exists and move it 3 units up on Z axis
cube_obj = bpy.data.objects.get("Cube")
if cube_obj:
    cube_obj.location.z += 3

# Save the modified scene to a new .blend file without altering the original
bpy.ops.wm.save_as_mainfile(filepath=r"C:/Users/KIRPIC~1/AppData/Local/Temp/blender-probe/out/GPTOSS20/task4.blend", copy=True)