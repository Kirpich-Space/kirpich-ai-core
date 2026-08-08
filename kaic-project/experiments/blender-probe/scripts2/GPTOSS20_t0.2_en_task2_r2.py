import bpy, os

# Delete default objects
bpy.ops.object.select_all(action='SELECT')
bpy.ops.object.delete(use_global=False)

# Create cube
bpy.ops.mesh.primitive_cube_add(size=2)
cube = bpy.context.active_object

# Create red material
mat = bpy.data.materials.new(name="RedMaterial")
mat.use_nodes = True
nodes = mat.node_tree.nodes
links = mat.node_tree.links
for node in nodes:
    nodes.remove(node)
diffuse = nodes.new(type='ShaderNodeBsdfDiffuse')
diffuse.inputs['Color'].default_value = (1.0, 0.0, 0.0, 1.0)
output = nodes.new(type='ShaderNodeOutputMaterial')
links.new(diffuse.outputs['BSDF'], output.inputs['Surface'])
cube.data.materials.append(mat)

# Set render settings
scene = bpy.context.scene
scene.render.image_settings.file_format = 'PNG'
scene.render.resolution_x = 320
scene.render.resolution_y = 240
scene.render.resolution_percentage = 100

output_path = r"C:/Users/KIRPIC~1/AppData/Local/Temp/blender-probe/out2/GPTOSS20_t0.2_en_r2/task2.png"
os.makedirs(os.path.dirname(output_path), exist_ok=True)
scene.render.filepath = output_path

# Render to file
bpy.ops.render.render(write_still=True)