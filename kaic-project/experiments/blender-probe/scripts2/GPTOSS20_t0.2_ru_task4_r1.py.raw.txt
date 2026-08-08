import bpy

# Load the existing blend file
bpy.ops.wm.open_mainfile(filepath=r"C:/Users/KIRPIC~1/AppData/Local/Temp/blender-probe/out2/GPTOSS20_t0.2_ru_r1/task1.blend")

# Find the object named "Cube" and move it up by 3 units
cube_obj = bpy.data.objects.get("Cube")
if cube_obj:
    cube_obj.location.z += 3

# Save the modified scene to a new file without altering the original
bpy.ops.wm.save_as_mainfile(filepath=r"C:/Users/KIRPIC~1/AppData/Local/Temp/blender-probe/out2/GPTOSS20_t0.2_ru_r1/task4.blend", copy=True)