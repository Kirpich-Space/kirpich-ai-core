import bpy

# Remove all existing objects
bpy.ops.object.select_all(action='SELECT')
bpy.ops.object.delete(use_global=False)

# Create a new cube at the world origin
mesh = bpy.data.meshes.new('CubeMesh')
obj = bpy.data.objects.new('Cube', mesh)
bpy.context.collection.objects.link(obj)
bpy.context.view_layer.objects.active = obj
obj.location = (0.0, 0.0, 0.0)

# Create cube geometry
bm = bmesh.new()
bmesh.ops.create_cube(bm, size=2.0)  # default cube size is 2 units
bm.to_mesh(mesh)
bm.free()

# Save the scene to the specified path
output_path = r"C:/Users/KIRPIC~1/AppData/Local/Temp/blender-probe/out2/GPTOSS20_t0.7_en_r1/task1.blend"
bpy.ops.wm.save_mainfile(filepath=output_path)